//! 应用配置、共享状态与文件日志。


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

// ---------------------------------------------------------------------------
// 日志
// ---------------------------------------------------------------------------

/// 滚动日志的单文件大小上限（100MB）。
pub const MAX_LOG_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// 日志输出目标。
pub enum LogTarget {
    /// 不记文件日志（控制台仍输出）。
    None,
    /// `--log FILE`：单文件追加（旧行为，不滚动）。
    Single(PathBuf),
    /// `--log-dir DIR`：按日期分目录 + 滚动，单文件上限 [`MAX_LOG_FILE_SIZE`]。
    /// 产出结构：`DIR/<YYYY-MM-DD>/converter-NNN.log`。
    RotatingDir(PathBuf),
}

enum LogMode {
    Single { path: PathBuf },
    Rotating { dir: PathBuf, date: String, index: u32, written: u64 },
}

/// 线程安全的文件日志器（追加写）。
pub struct Logger {
    mode: Mutex<LogMode>,
    max_file_size: u64,
}

static LOGGER: OnceLock<Option<Logger>> = OnceLock::new();

pub fn init_logger(target: LogTarget) {
    let logger = match target {
        LogTarget::None => None,
        LogTarget::Single(path) => Some(Logger {
            mode: Mutex::new(LogMode::Single { path }),
            max_file_size: MAX_LOG_FILE_SIZE,
        }),
        LogTarget::RotatingDir(dir) => Some(Logger {
            mode: Mutex::new(new_rotating_mode(dir, MAX_LOG_FILE_SIZE)),
            max_file_size: MAX_LOG_FILE_SIZE,
        }),
    };
    let _ = LOGGER.set(logger);
}

/// 初始化滚动日志模式：定位当天目录里最新的序号并续写（重启不覆盖旧文件）；
/// 若该文件已超限则从下一个序号开始。
fn new_rotating_mode(dir: PathBuf, max_file_size: u64) -> LogMode {
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let day_dir = dir.join(&date);
    let _ = std::fs::create_dir_all(&day_dir);
    let mut max_index: u32 = 0;
    if let Ok(rd) = std::fs::read_dir(&day_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name
                .strip_prefix("converter-")
                .and_then(|r| r.strip_suffix(".log"))
            {
                if let Ok(i) = rest.parse::<u32>() {
                    max_index = max_index.max(i);
                }
            }
        }
    }
    if max_index == 0 {
        return LogMode::Rotating { dir, date, index: 1, written: 0 };
    }
    let cur = day_dir.join(format!("converter-{max_index:03}.log"));
    let written = std::fs::metadata(&cur).map(|m| m.len()).unwrap_or(0);
    if written >= max_file_size {
        LogMode::Rotating { dir, date, index: max_index + 1, written: 0 }
    } else {
        LogMode::Rotating { dir, date, index: max_index, written }
    }
}

/// 每条日志同时写控制台（stderr）与日志文件（若已配置）。
pub fn log(msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!("[{ts}] {msg}\n");
    eprint!("{line}");
    if let Some(Some(logger)) = LOGGER.get().map(|o| o.as_ref()) {
        logger.write_line(&line);
    }
}

/// 拼接错误的完整 `source()` 链：`顶层: 来源: 根因`。
///
/// reqwest 的 Display 只给顶层消息（如 `error sending request for url (...)`），
/// 真实原因（连接拒绝 / DNS 失败 / TLS 握手错误）挂在 source 链上，不展开就无法定位。
pub fn err_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![e.to_string()];
    let mut src = e.source();
    while let Some(s) = src {
        parts.push(s.to_string());
        src = s.source();
    }
    parts.join(": ")
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
    fn write_line(&self, line: &str) {
        // 锁中毒时继续写（日志不应因 panic 链而停摆）
        let mut mode = match self.mode.lock() {
            Ok(m) => m,
            Err(p) => p.into_inner(),
        };
        match &mut *mode {
            LogMode::Single { path } => {
                let _ = append_line(path, line);
            }
            LogMode::Rotating { dir, date, index, written } => {
                // 跨天切换目录
                let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                if today != *date {
                    *date = today;
                    *index = 1;
                    *written = 0;
                }
                // 超限滚动：converter-001.log → converter-002.log
                if *written + line.len() as u64 > self.max_file_size {
                    *index += 1;
                    *written = 0;
                }
                let day_dir = dir.join(&*date);
                if std::fs::create_dir_all(&day_dir).is_err() {
                    return;
                }
                let file = day_dir.join(format!("converter-{index:03}.log"));
                if append_line(&file, line).is_ok() {
                    *written += line.len() as u64;
                }
            }
        }
    }
}

fn append_line(path: &PathBuf, line: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())
}

/// 共享应用状态。
pub struct AppState {
    pub config: AppConfig,
    /// 上游注册表（内部 RwLock，支持管理后台 reload 热替换）。
    pub upstreams: crate::upstream::UpstreamRegistry,
    /// 重建注册表所需的基础设置（CLI/env；reload 时作为文件未覆盖字段的兜底）。
    pub registry_base: crate::admin::config_store::BaseSettings,
    /// 请求启动时的 CLI 默认上游（文件配置可能覆盖，实际默认以 registry 为准）。
    pub cli_default_upstream: String,
    pub http: reqwest::Client,
    /// 请求统计（内存聚合 + 周期落盘）。
    pub metrics: Arc<crate::admin::MetricsStore>,
    /// 数据目录（统计 / 签到记录 / 后台配置）。
    pub data_dir: PathBuf,
    /// 是否启用管理后台（/admin/*）。
    pub admin_enabled: bool,
    /// 滚动日志目录（--log-dir；后台日志查看读取此处）。
    pub log_dir: Option<PathBuf>,
}

impl AppState {
    /// 按名字取上游；名字为 `None` 或空串时取默认上游。
    ///
    /// 兼容旧命名：路径前缀 `codebuddy` / `workbuddy` 均映射到 `buddy`
    /// （2026-10 合并前的旧 base_url 不失效）。
    pub fn upstream(&self, name: Option<&str>) -> Option<Arc<dyn Upstream>> {
        let mapped = name.map(|n| match n {
            "codebuddy" | "workbuddy" => "buddy",
            other => other,
        });
        self.upstreams.get(mapped)
    }

    /// 当前默认上游名（registry 内的，可能被后台配置覆盖）。
    pub fn default_upstream(&self) -> String {
        self.upstreams.default_name()
    }

    /// 上游注册表快照（用于管理后台列举）。
    pub(crate) fn upstreams_snapshot(&self) -> Vec<(String, Arc<dyn Upstream>)> {
        self.upstreams.snapshot()
    }
}

pub type SharedState = std::sync::Arc<AppState>;

/// 4 字节随机 hex，作为请求 id。
pub fn new_rid() -> String {
    crate::rand_hex(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "c2a-logtest-{tag}-{}",
            crate::rand_hex(4)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn test_single_mode_appends() {
        let dir = temp_dir("single");
        let path = dir.join("a.log");
        let logger = Logger {
            mode: Mutex::new(LogMode::Single { path: path.clone() }),
            max_file_size: MAX_LOG_FILE_SIZE,
        };
        logger.write_line("[t] one\n");
        logger.write_line("[t] two\n");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("one"));
        assert!(text.contains("two"));
        assert_eq!(text.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rotating_creates_dated_dir_and_rolls() {
        let dir = temp_dir("rot");
        let logger = Logger {
            mode: Mutex::new(new_rotating_mode(dir.clone(), 200)),
            max_file_size: 200,
        };
        // 每行约 30+ 字节，写 20 行必然触发滚动
        for i in 0..20 {
            logger.write_line(&format!("[t] line-{i}-{}\n", "x".repeat(20)));
        }
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let day_dir = dir.join(&date);
        let mut files: Vec<String> = std::fs::read_dir(&day_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        files.sort();
        assert!(files.len() >= 2, "应产生多个滚动文件，实际: {files:?}");
        assert_eq!(files[0], "converter-001.log");
        // 每个文件都不超过上限（留一行余量）
        for f in &files {
            let len = std::fs::metadata(day_dir.join(f)).unwrap().len();
            assert!(len <= 200 + 60, "{f} 超限: {len}");
        }
        // 内容完整（无丢失）
        let mut total_lines = 0;
        for f in &files {
            let text = std::fs::read_to_string(day_dir.join(f)).unwrap();
            total_lines += text.lines().count();
        }
        assert_eq!(total_lines, 20);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rotating_resumes_on_restart_without_overwrite() {
        let dir = temp_dir("resume");
        // 第一次运行：写几行
        {
            let logger = Logger {
                mode: Mutex::new(new_rotating_mode(dir.clone(), u64::MAX)),
                max_file_size: u64::MAX,
            };
            logger.write_line("[t] first-run\n");
        }
        // 第二次运行（模拟重启）：应续写同一文件而非覆盖
        {
            let logger = Logger {
                mode: Mutex::new(new_rotating_mode(dir.clone(), u64::MAX)),
                max_file_size: u64::MAX,
            };
            logger.write_line("[t] second-run\n");
        }
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let text =
            std::fs::read_to_string(dir.join(&date).join("converter-001.log")).unwrap();
        assert!(text.contains("first-run"));
        assert!(text.contains("second-run"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rotating_starts_new_file_when_current_full() {
        let dir = temp_dir("full");
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let day_dir = dir.join(&date);
        std::fs::create_dir_all(&day_dir).unwrap();
        // 预置一个已超限的 001 文件
        std::fs::write(day_dir.join("converter-001.log"), "x".repeat(300)).unwrap();
        let logger = Logger {
            mode: Mutex::new(new_rotating_mode(dir.clone(), 200)),
            max_file_size: 200,
        };
        logger.write_line("[t] fresh\n");
        let text = std::fs::read_to_string(day_dir.join("converter-002.log")).unwrap();
        assert!(text.contains("fresh"));
        // 001 未被改动
        assert_eq!(
            std::fs::metadata(day_dir.join("converter-001.log")).unwrap().len(),
            300
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
