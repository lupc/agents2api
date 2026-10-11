//! 管理后台：WebAPI + 内嵌前端。
//!
//! 挂在 `/admin/*` 下，与 `/v1/*` 完全隔离：

pub mod assets;
pub mod buddy_billing;
pub mod checkin;
pub mod config_store;
pub mod credits;
pub mod db;
pub mod metrics;
pub mod routes;
pub mod speed;

use std::path::PathBuf;

use axum::Router;
use axum::routing::{get, post};

use crate::config::SharedState;

pub use metrics::MetricsStore;

/// 后台运行时共享状态（挂在 axum State 上，独立于 AppState 以便只传需要的部分）。
#[derive(Clone)]
pub struct AdminState {
    pub app: SharedState,
    /// 数据目录（统计、签到记录、上游配置文件的点）。
    pub data_dir: PathBuf,
    /// 事件记录数据库句柄（查询用）。
    pub db: db::Db,
    /// 事件记录写入句柄（非阻塞入队）。
    pub recorder: db::Recorder,
}

/// 默认数据目录：优先程序目录下的 `data/`（配置随程序走，便于便携部署）；
/// 程序目录不可写（如安装到只读位置）时回退平台标准目录：
/// `%APPDATA%\agents2api`（Windows）/ `~/.local/share/agents2api`（*nix）/ macOS Application Support。
pub fn default_data_dir() -> PathBuf {
    // 优先：程序目录/data（可写性检测）
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("data");
            if dir_writable(&candidate) {
                return candidate;
            }
        }
    }
    // 回退：平台标准目录
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    if cfg!(target_os = "windows") {
        let base = std::env::var("APPDATA")
            .unwrap_or_else(|_| format!("{home}\\AppData\\Roaming"));
        PathBuf::from(base).join("agents2api")
    } else if cfg!(target_os = "macos") {
        PathBuf::from(&home)
            .join("Library")
            .join("Application Support")
            .join("agents2api")
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(&home).join(".local").join("share"))
            .join("agents2api")
    }
}

/// 目录是否可创建且可写（用于判断程序目录能否承载数据）。
fn dir_writable(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-test");
    match std::fs::write(&probe, b"ok") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 组装 `/admin` 子路由（含 API 与静态资源）。
pub fn router(state: AdminState) -> Router<SharedState> {
    Router::new()
        .route("/", get(assets::index))
        .route("/api/health", get(routes::health))
        .route("/api/overview", get(routes::overview))
        .route("/api/stats/usage", get(routes::stats_usage))
        .route("/api/stats/models", get(routes::stats_models))
        .route("/api/stats/speed", get(routes::stats_speed))
        .route("/api/upstreams", get(routes::list_upstreams))
        .route("/api/upstreams/reload", post(routes::reload_upstreams))
        .route("/api/upstreams/{name}", get(routes::upstream_detail))
        .route("/api/models", get(routes::all_models))
        .route("/api/sync", post(routes::sync_models))
        .route("/api/config", get(routes::get_config).put(routes::put_config))
        .route(
            "/api/settings",
            get(routes::get_settings).put(routes::put_settings),
        )
        .route("/api/checkin/trae", post(routes::checkin_trae))
        .route("/api/checkin/buddy", post(routes::checkin_buddy))
        .route("/api/checkin/all", post(routes::checkin_all))
        .route("/api/checkin/results", get(routes::checkin_results))
        .route("/api/credits/trae", get(routes::credits_trae))
        .route("/api/credits/buddy", get(routes::credits_buddy))
        .route("/api/logs", get(routes::logs_tail))
        // SPA：其余 /admin/* 路径回前端入口
        .fallback(get(assets::spa_fallback))
        .with_state(state)
}


