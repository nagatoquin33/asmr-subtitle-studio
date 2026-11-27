//! ggml 模型下载管理：whisper.cpp 格式模型 + Silero VAD 模型。
//! 源站链：hf-mirror.com（国内直连）→ huggingface.co 兜底；支持断点续传与实时进度。

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use parking_lot::RwLock;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Semaphore};

use crate::paths::Paths;

const HF_MIRROR: &str = "https://hf-mirror.com";
const HF: &str = "https://huggingface.co";
const UA: &str = "asmr-studio/2.0";

pub struct ModelSpec {
    pub name: &'static str,
    pub file: &'static str,
    pub repo: &'static str,
    pub size_hint: &'static str,
    pub desc: &'static str,
}

pub const REGISTRY: &[ModelSpec] = &[
    ModelSpec {
        name: "small",
        file: "ggml-small.bin",
        repo: "ggerganov/whisper.cpp",
        size_hint: "~465MB",
        desc: "速度快，中英日准确率良好（默认）",
    },
    ModelSpec {
        name: "medium",
        file: "ggml-medium.bin",
        repo: "ggerganov/whisper.cpp",
        size_hint: "~1.5GB",
        desc: "均衡之选，准确率更高",
    },
    ModelSpec {
        name: "large-v3",
        file: "ggml-large-v3.bin",
        repo: "ggerganov/whisper.cpp",
        size_hint: "~3GB",
        desc: "最高准确率，推荐 GPU 使用",
    },
    ModelSpec {
        name: "large-v3-turbo",
        file: "ggml-large-v3-turbo.bin",
        repo: "ggerganov/whisper.cpp",
        size_hint: "~1.6GB",
        desc: "large-v3 精简加速版，速度/精度折中最优",
    },
];

pub fn spec(name: &str) -> Option<&'static ModelSpec> {
    REGISTRY.iter().find(|m| m.name == name)
}

#[derive(Clone)]
struct DlState {
    status: String, // idle / downloading / ready / error
    progress: Option<f64>,
    detail: String,
    error: String,
    downloaded: u64,
    total: u64,
}

impl Default for DlState {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            progress: None,
            detail: String::new(),
            error: String::new(),
            downloaded: 0,
            total: 0,
        }
    }
}

pub struct ModelStore {
    paths: Paths,
    http: reqwest::Client,
    states: RwLock<HashMap<String, DlState>>,
    locks: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// 注入的 runtime 句柄：同步上下文（如 Tauri 同步命令线程）里
    /// 直接 tokio::spawn 会 panic（no reactor），必须用 Handle::spawn。
    runtime: tokio::runtime::Handle,
}

impl ModelStore {
    pub fn new(paths: Paths, runtime: tokio::runtime::Handle) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Arc::new(Self {
            paths,
            http,
            states: RwLock::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            runtime,
        })
    }

    pub fn model_file(&self, name: &str) -> PathBuf {
        self.paths.model_dir(name).join("model.bin")
    }

    /// ggml 文件头魔数（whisper.cpp 模型以 "lmgg"（0x67676d6c LE）开头）。
    fn is_ggml_file(path: &std::path::Path) -> bool {
        use std::io::Read;
        let Ok(mut f) = std::fs::File::open(path) else { return false };
        let mut head = [0u8; 4];
        f.read_exact(&mut head).is_ok() && &head == b"lmgg"
    }

    pub fn is_ready(&self, name: &str) -> bool {
        let f = self.model_file(name);
        f.is_file() && Self::is_ggml_file(&f)
    }

    fn set_state(&self, name: &str, f: impl FnOnce(&mut DlState)) {
        let mut states = self.states.write();
        let st = states.entry(name.to_string()).or_default();
        f(st);
        let snapshot = st.clone();
        drop(states);
        // 状态镜像写入 task 事件通道由上层桥接；此处仅内存态（UI 轮询读取）
        let _ = snapshot;
    }

    pub fn status(&self, name: &str) -> Value {
        let Some(info) = spec(name) else { return json!({}) };
        let ready = self.is_ready(name);
        let mut out = json!({
            "name": info.name,
            "repos": [format!("{}/{}", HF_MIRROR, info.repo), format!("{}/{}", HF, info.repo)],
            "size_hint": info.size_hint,
            "desc": info.desc,
            "local_path": if ready { json!(self.model_file(name).to_string_lossy().to_string()) } else { Value::Null },
        });
        if ready {
            out["status"] = json!("ready");
            out["progress"] = json!(1.0);
            out["detail"] = json!("");
            out["error"] = json!("");
        } else {
            let st = self.states.read().get(name).cloned().unwrap_or_default();
            out["status"] = json!(st.status);
            out["progress"] = if st.progress.is_some() { json!(st.progress.unwrap()) } else { Value::Null };
            out["detail"] = json!(st.detail);
            out["error"] = json!(st.error);
            if st.status == "downloading" && st.total > 0 {
                out["detail"] = json!(format!("{:.0}/{:.0}MB", st.downloaded as f64 / 1048576.0, st.total as f64 / 1048576.0));
            }
        }
        out
    }

    pub fn all_status(&self) -> Vec<Value> {
        REGISTRY.iter().map(|m| self.status(m.name)).collect()
    }

    /// 异步启动下载（UI 触发）。
    pub fn start_download(self: &Arc<Self>, name: &str) -> Result<()> {
        if spec(name).is_none() {
            return Err(anyhow!("未知模型: {}", name));
        }
        if self.is_ready(name) {
            return Ok(());
        }
        let store = Arc::clone(self);
        let name = name.to_string();
        // 必须经 Handle::spawn：本方法可能在无 reactor 的线程（同步命令）上被调用
        self.runtime.spawn(async move {
            if let Err(e) = store.ensure_ready(&name).await {
                store.set_state(&name, |st| {
                    st.status = "error".into();
                    st.progress = None;
                    st.error = format!("{:#}", e).chars().take(300).collect();
                });
            }
        });
        Ok(())
    }

    /// 阻塞直到模型就绪；同一模型并发调用串行等待同一锁。
    pub async fn ensure_ready(self: &Arc<Self>, name: &str) -> Result<PathBuf> {
        let sem = {
            let mut locks = self.locks.lock().await;
            Arc::clone(locks.entry(name.to_string()).or_insert_with(|| Arc::new(Semaphore::new(1))))
        };
        let _permit = sem.acquire().await.expect("download lock poisoned");
        if self.is_ready(name) {
            self.set_state(name, |st| {
                st.status = "ready".into();
                st.progress = Some(1.0);
            });
            return Ok(self.model_file(name));
        }
        self.download(name).await?;
        Ok(self.model_file(name))
    }

    async fn download(&self, name: &str) -> Result<()> {
        let Some(info) = spec(name) else { return Err(anyhow!("未知模型: {}", name)) };
        self.set_state(name, |st| {
            st.status = "downloading".into();
            st.progress = Some(0.0);
            st.detail = "正在连接模型源…".into();
            st.error.clear();
            st.downloaded = 0;
            st.total = 0;
        });

        let dest = self.model_file(name);
        std::fs::create_dir_all(dest.parent().unwrap())?;
        let part = dest.with_extension("bin.part");

        let mut last_err: Option<anyhow::Error> = None;
        for base in [HF_MIRROR, HF] {
            let url = format!("{}/{}/resolve/main/{}", base, info.repo, info.file);
            match self.download_one(&url, &part, name).await {
                Ok(()) => {
                    std::fs::rename(&part, &dest)?;
                    self.set_state(name, |st| {
                        st.status = "ready".into();
                        st.progress = Some(1.0);
                        st.detail.clear();
                        st.error.clear();
                    });
                    return Ok(());
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        let msg = last_err.map(|e| format!("{:#}", e)).unwrap_or_default();
        self.set_state(name, |st| {
            st.status = "error".into();
            st.progress = None;
            st.error = msg.chars().take(300).collect();
        });
        Err(anyhow!("模型 {} 所有源下载失败: {}", name, msg))
    }

    /// 单文件下载：同源断点续传重试（镜像大文件常中途断流），
    /// 仅在连接彻底不可建/HTTP 4xx 时换源；分块读超时 45s 视为断流。
    async fn download_one(&self, url: &str, part: &PathBuf, name: &str) -> Result<()> {
        const MAX_ATTEMPTS: u32 = 8;
        let mut attempt: u32 = 0;
        let mut fatal = false; // true = 换源
        loop {
            attempt += 1;
            if fatal || attempt > MAX_ATTEMPTS {
                return Err(anyhow!(if fatal {
                    format!("HTTP 不可恢复错误: {}", url)
                } else {
                    format!("重试 {} 次后仍中断: {}", MAX_ATTEMPTS, url)
                }));
            }
            let pos: u64 = part.metadata().map(|m| m.len()).unwrap_or(0);
            let mut req = self.http.get(url);
            if pos > 0 {
                req = req.header("Range", format!("bytes={}-", pos));
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    // 连接层错误：短暂退避后同源续传（不换源——HF 官方源国内不通，
                    // 换源等于放弃；hf-mirror 断流是常态，续传即可推进）
                    eprintln!("[models] {} 第 {} 次连接失败: {:#}，续传重试", name, attempt, e);
                    tokio::time::sleep(Duration::from_secs(2u64.saturating_mul(attempt.min(4) as u64))).await;
                    continue;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let code = status.as_u16();
                if code == 404 || code == 401 || code == 403 {
                    fatal = true; // 换源
                    continue;
                }
                eprintln!("[models] HTTP {}，退避重试", code);
                tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
                continue;
            }
            let resumed = status.as_u16() == 206;
            if !resumed && pos > 0 {
                let _ = fs::remove_file(part); // 服务端不支持续传，重下
            }
            let total = resp.content_length().map(|t| if resumed { t + pos } else { t }).unwrap_or(0);
            if total > 0 {
                self.set_state(name, |st| {
                    st.total = total;
                    st.detail = format!("{:.0}MB", total as f64 / 1048576.0);
                });
            }

            // write 模式打开（Windows 上 append 句柄无法 set_len，会报 os error 5）
            let mut file = match tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(part)
                .await
            {
                Ok(f) => f,
                Err(e) => return Err(anyhow!("打开临时文件失败: {}", e)),
            };
            if resumed {
                use tokio::io::{AsyncSeekExt, SeekFrom};
                file.seek(SeekFrom::End(0)).await?;
            } else {
                file.set_len(0).await?;
            }
            let mut downloaded = if resumed { pos } else { 0 };
            let mut last_beat = 0u64;
            let mut stream = resp.bytes_stream();
            loop {
                // 分块读超时：45s 无数据视为断流 → 跳出续传
                let chunk = match tokio::time::timeout(Duration::from_secs(45), stream.next()).await {
                    Err(_) => {
                        eprintln!("[models] {} 读取超时（45s 无数据），续传重试", name);
                        break;
                    }
                    Ok(None) => {
                        // 流正常结束
                        file.flush().await?;
                        self.set_state(name, |st| {
                            st.downloaded = downloaded;
                            st.progress = Some(1.0);
                        });
                        return Ok(());
                    }
                    Ok(Some(Err(e))) => {
                        eprintln!("[models] {} 传输中断: {:#}，续传重试", name, e);
                        break;
                    }
                    Ok(Some(Ok(c))) => c,
                };
                if let Err(e) = file.write_all(&chunk).await {
                    return Err(anyhow!("写入失败: {}", e));
                }
                downloaded += chunk.len() as u64;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if now - last_beat > 400 {
                    last_beat = now;
                    self.set_state(name, |st| {
                        st.downloaded = downloaded;
                        if total > 0 {
                            st.progress = Some((downloaded as f64 / total as f64).min(1.0));
                        }
                    });
                }
            }
            // 断流：回到 loop 头按 .part 现状续传
            file.flush().await?;
        }
    }

    // ---------- VAD 模型（Silero，~885KB） ----------

    pub fn vad_ready(&self) -> bool {
        self.paths.vad_model_path().is_file()
    }

    /// 确保 VAD 模型存在（缺失时自动下载；静默小文件）。
    pub async fn ensure_vad(self: &Arc<Self>) -> Result<PathBuf> {
        let dest = self.paths.vad_model_path();
        if dest.is_file() {
            return Ok(dest);
        }
        std::fs::create_dir_all(dest.parent().unwrap())?;
        let part = dest.with_extension("part");
        let mut last_err = None;
        for base in [HF_MIRROR, HF] {
            let url = format!("{}/ggml-org/whisper-vad/resolve/main/{}", base, dest.file_name().unwrap().to_string_lossy());
            match self.download_one(&url, &part, "vad").await {
                Ok(()) => {
                    std::fs::rename(&part, &dest)?;
                    return Ok(dest);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("VAD 模型下载失败")))
    }
}
