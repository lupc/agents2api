//! 应用配置、共享状态与文件日志。

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use crate::upstream::Upstream;

#[derive(Clone)]
pub struct AppConfig {
    pub api_key: String,
    pub desensitize: bool,
    pub no_compact: bool,
}

/// 写文件日志（追加，带时间戳）。未配置路径时丢弃。
pub struct Logger {
    path: PathBuf,
    lock: Mutex<()>,
}

static LOGGER: OnceLock<Option<Logger>> = OnceLock::new();

pub fn init_logger(path: Option<PathBuf>) {
    let _ = LOGGER.set(path.map(|p| Logger {
        path: p,
        lock: Mutex::new(()),
    }));
}

pub fn log(msg: &str) {
    if let Some(Some(logger)) = LOGGER.get().map(|o| o.as_ref()) {
        logger.write(msg);
    }
}

/// 带请求 id 前缀的日志便捷封装。
pub fn log_rid(rid: &str, msg: &str) {
    if rid.is_empty() {
        log(msg);
    } else {
        log(&format!("[{rid}] {msg}"));
    }
}

impl Logger {
    fn write(&self, msg: &str) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("[{ts}] {msg}\n");
        let _guard = self.lock.lock().unwrap();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// 共享应用状态。
pub struct AppState {
    pub config: AppConfig,
    /// 已配置的上游（key = 上游名，如 `codebuddy` / `trae`）。
    pub upstreams: HashMap<String, Arc<dyn Upstream>>,
    /// 无路径前缀时使用的默认上游名。
    pub default_upstream: String,
    pub http: reqwest::Client,
}

impl AppState {
    /// 按名字取上游；名字为 `None` 或空串时取默认上游。
    pub fn upstream(&self, name: Option<&str>) -> Option<Arc<dyn Upstream>> {
        let key = match name {
            Some(n) if !n.is_empty() => n,
            _ => self.default_upstream.as_str(),
        };
        self.upstreams.get(key).cloned()
    }
}

pub type SharedState = std::sync::Arc<AppState>;

/// 4 字节随机 hex，作为请求 id。
pub fn new_rid() -> String {
    crate::rand_hex(4)
}
