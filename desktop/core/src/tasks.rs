//! 任务管理器：注册表、持久化（节流）、事件发布、工作目录生命周期。
//! 状态变更 → update() → ① broadcast 事件（UI 订阅）→ ② task.json 落盘。

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use chrono::Local;
use parking_lot::{Mutex, RwLock};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::paths::{sanitize_filename, Paths};

pub const RUNNING_STAGES: &[&str] = &[
    "queued", "downloading", "extracting", "asr", "translating", "proofreading", "writing",
];
pub const FINAL_STAGES: &[&str] = &["done", "error", "canceled"];

const PERSIST_MIN_INTERVAL_MS: u64 = 2000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_video: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub status: String,
    pub stage: String,
    pub progress: f64,
    pub message: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub cancel: bool,
    pub created_at: String,
    pub updated_at: String,
    pub dir: String,
    pub media: MediaInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default)]
    pub asr: Value,
    #[serde(default)]
    pub options: Value,
    pub counts: Counts,
    #[serde(default)]
    pub has_result: bool,
    #[serde(default)]
    pub outputs: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Counts {
    pub segments: u64,
    pub translated: u64,
}

fn now_str() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

pub fn new_task_id() -> String {
    let mut buf = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut buf);
    format!(
        "t_{}_{}",
        Local::now().format("%Y%m%d_%H%M%S"),
        buf.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    )
}

#[derive(Clone)]
pub enum EventKind {
    Progress,
    Done,
    Error,
    Canceled,
}

impl EventKind {
    fn as_str(&self) -> &'static str {
        match self {
            EventKind::Progress => "progress",
            EventKind::Done => "done",
            EventKind::Error => "error",
            EventKind::Canceled => "canceled",
        }
    }
}

pub struct TaskManager {
    paths: Paths,
    tasks: RwLock<HashMap<String, Task>>,
    last_persist: Mutex<HashMap<String, u64>>,
    bus: broadcast::Sender<Value>,
    started: std::time::Instant,
}

impl TaskManager {
    pub fn new(paths: Paths) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(256);
        let mgr = Arc::new(Self {
            tasks: RwLock::new(HashMap::new()),
            last_persist: Mutex::new(HashMap::new()),
            bus: tx,
            started: std::time::Instant::now(),
            paths,
        });
        mgr.load_existing();
        mgr
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.bus.subscribe()
    }

    fn load_existing(&self) {
        let _ = self.paths.ensure_dirs();
        let Ok(entries) = fs::read_dir(self.paths.tasks()) else {
            return;
        };
        let mut dirs: Vec<_> = entries.flatten().collect();
        dirs.sort_by_key(|e| e.file_name());
        for entry in dirs {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("t_") {
                continue;
            }
            let meta = entry.path().join("task.json");
            let Ok(text) = fs::read_to_string(&meta) else { continue };
            let Ok(mut task) = serde_json::from_str::<Task>(&text) else { continue };
            if RUNNING_STAGES.contains(&task.status.as_str()) {
                task.status = "error".into();
                task.message = "服务重启导致中断，可点击重试".into();
            }
            task.dir = self.paths.tasks().join(&task.id).to_string_lossy().to_string();
            self.tasks.write().insert(task.id.clone(), task);
        }
    }

    fn persist(&self, id: &str) {
        let Some(task) = self.tasks.read().get(id).cloned() else { return };
        let dir = self.paths.tasks().join(id);
        let _ = fs::create_dir_all(&dir);
        let tmp = dir.join("task.json.tmp");
        if let Ok(text) = serde_json::to_string(&task) {
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, dir.join("task.json"));
            }
        }
        self.last_persist
            .lock()
            .insert(id.to_string(), self.now_ms());
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// 更新任务字段并广播。important=true 时立即落盘，否则节流。
    pub fn update<F>(&self, id: &str, important: bool, kind: EventKind, mut f: F) -> Option<Task>
    where
        F: FnMut(&mut Task),
    {
        let mut writer = self.tasks.write();
        let task = writer.get_mut(id)?;
        f(task);
        task.updated_at = now_str();
        let snapshot = task.clone();
        drop(writer);

        let should_persist = important || {
            let last = self.last_persist.lock().get(id).copied().unwrap_or(0);
            self.now_ms().saturating_sub(last) >= PERSIST_MIN_INTERVAL_MS
        };
        if should_persist {
            self.persist(id);
        }
        let _ = self.bus.send(json!({"type": kind.as_str(), "task": snapshot}));
        Some(snapshot)
    }

    pub fn emit_final(&self, id: &str) {
        let status = self.tasks.read().get(id).map(|t| t.status.clone());
        let kind = match status.as_deref() {
            Some("done") => EventKind::Done,
            Some("error") => EventKind::Error,
            Some("canceled") => EventKind::Canceled,
            _ => EventKind::Progress,
        };
        let task = self.tasks.read().get(id).cloned();
        if let Some(task) = task {
            let _ = self.bus.send(json!({"type": kind.as_str(), "task": task}));
        }
    }

    pub fn get(&self, id: &str) -> Option<Task> {
        self.tasks.read().get(id).cloned()
    }

    pub fn list(&self, limit: usize) -> Vec<Task> {
        let mut out: Vec<Task> = self.tasks.read().values().cloned().collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out.truncate(limit);
        out
    }

    pub fn is_cancelled(&self, id: &str) -> bool {
        self.tasks.read().get(id).map(|t| t.cancel).unwrap_or(false)
    }

    pub fn create_from_path(&self, src: &std::path::Path, options: Value) -> Task {
        let id = new_task_id();
        let dir = self.paths.tasks().join(&id);
        let _ = fs::create_dir_all(&dir);
        let media_path = src.to_path_buf();
        let now = now_str();
        let task = Task {
            id: id.clone(),
            name: media_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| id.clone()),
            status: "queued".into(),
            stage: "queued".into(),
            progress: 0.0,
            message: "排队中".into(),
            error: None,
            cancel: false,
            created_at: now.clone(),
            updated_at: now,
            dir: dir.to_string_lossy().to_string(),
            media: MediaInfo {
                path: Some(media_path.to_string_lossy().to_string()),
                duration: None,
                size: None,
                has_video: None,
            },
            source_url: None,
            asr: Value::Null,
            options,
            counts: Counts::default(),
            has_result: false,
            outputs: json!({}),
            output_dir: None,
        };
        self.tasks.write().insert(id.clone(), task.clone());
        self.persist(&id);
        let _ = self.bus.send(json!({"type": "progress", "task": task}));
        task
    }

    pub fn create_url(&self, url: &str, options: Value) -> Task {
        let id = new_task_id();
        let dir = self.paths.tasks().join(&id);
        let _ = fs::create_dir_all(&dir);
        let now = now_str();
        let name = if url.chars().count() <= 72 {
            url.to_string()
        } else {
            let prefix: String = url.chars().take(69).collect();
            format!("{}…", prefix)
        };
        let task = Task {
            id: id.clone(),
            name,
            status: "queued".into(),
            stage: "queued".into(),
            progress: 0.0,
            message: "排队中".into(),
            error: None,
            cancel: false,
            created_at: now.clone(),
            updated_at: now,
            dir: dir.to_string_lossy().to_string(),
            media: MediaInfo::default(),
            source_url: Some(url.to_string()),
            asr: Value::Null,
            options,
            counts: Counts::default(),
            has_result: false,
            outputs: json!({}),
            output_dir: None,
        };
        self.tasks.write().insert(id.clone(), task.clone());
        self.persist(&id);
        let _ = self.bus.send(json!({"type": "progress", "task": task}));
        task
    }

    pub fn cancel(&self, id: &str) -> Option<Task> {
        {
            let mut writer = self.tasks.write();
            let task = writer.get_mut(id)?;
            if task.status == "queued" {
                task.status = "canceled".into();
                task.stage = "canceled".into();
                task.message = "已取消".into();
                let snap = task.clone();
                drop(writer);
                self.persist(id);
                let _ = self.bus.send(json!({"type": "canceled", "task": snap}));
                return Some(snap);
            }
            task.cancel = true;
        }
        self.persist(id);
        self.get(id)
    }

    /// 删除任务（运行中拒绝）；连带删除工作目录与 output 产物目录。
    pub fn delete(&self, id: &str) -> Result<(), String> {
        let task = {
            let mut writer = self.tasks.write();
            let Some(task) = writer.get_mut(id) else { return Err("任务不存在".into()) };
            if RUNNING_STAGES.contains(&task.status.as_str()) && task.status != "queued" {
                return Err("任务运行中，请先取消".into());
            }
            if task.status == "queued" {
                task.status = "canceled".into();
                task.cancel = true;
            }
            task.clone()
        };
        self.tasks.write().remove(id);
        self.last_persist.lock().remove(id);
        let _ = fs::remove_dir_all(self.paths.tasks().join(id));
        if let Some(out) = &task.output_dir {
            let out = std::path::PathBuf::from(out);
            let canonical_output = self.paths.output().canonicalize().unwrap_or_default();
            if out
                .canonicalize()
                .map(|p| p.parent().map(|pp| pp == canonical_output).unwrap_or(false))
                .unwrap_or(false)
            {
                let _ = fs::remove_dir_all(out);
            }
        }
        Ok(())
    }

    pub fn tasks_dir(&self) -> std::path::PathBuf {
        self.paths.tasks().clone()
    }
}

/// 计划产物目录：output/{视频名}/，重名追加任务 id 后缀。
pub fn plan_output_dir(paths: &Paths, task_id: &str, name: &str) -> std::path::PathBuf {
    let stem = std::path::Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| task_id.to_string());
    let safe: String = sanitize_filename(&stem).chars().take(60).collect();
    let safe = if safe.is_empty() {
        task_id.chars().rev().take(6).collect::<String>()
    } else {
        safe
    };
    let mut out_dir = paths.output().join(&safe);
    if out_dir.exists() && fs::read_dir(&out_dir).map(|mut d| d.next().is_some()).unwrap_or(false) {
        let suffix: String = task_id.chars().rev().take(6).collect();
        out_dir = paths.output().join(format!("{}_{}", safe, suffix));
    }
    out_dir
}
