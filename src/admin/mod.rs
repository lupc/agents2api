//! 管理后台：WebAPI + 内嵌前端。
//!
//! 挂在 `/admin/*` 下，与 `/v1/*` 完全隔离：

pub mod assets;
pub mod checkin;
pub mod config_store;
pub mod credits;
pub mod metrics;
pub mod routes;

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
}

/// 默认数据目录：`%APPDATA%\agents2api`（Windows）/ `~/.local/share/agents2api`（*nix）/ macOS Application Support。
pub fn default_data_dir() -> PathBuf {
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

/// 组装 `/admin` 子路由（含 API 与静态资源）。
pub fn router(state: AdminState) -> Router<SharedState> {
    Router::new()
        .route("/", get(assets::index))
        .route("/api/health", get(routes::health))
        .route("/api/overview", get(routes::overview))
        .route("/api/stats/usage", get(routes::stats_usage))
        .route("/api/stats/models", get(routes::stats_models))
        .route("/api/upstreams", get(routes::list_upstreams))
        .route("/api/upstreams/reload", post(routes::reload_upstreams))
        .route("/api/upstreams/{name}", get(routes::upstream_detail))
        .route("/api/models", get(routes::all_models))
        .route("/api/sync", post(routes::sync_models))
        .route("/api/config", get(routes::get_config).put(routes::put_config))
        .route("/api/checkin/trae", post(routes::checkin_trae))
        .route("/api/checkin/results", get(routes::checkin_results))
        .route("/api/credits/trae", get(routes::credits_trae))
        .route("/api/logs", get(routes::logs_tail))
        // SPA：其余 /admin/* 路径回前端入口
        .fallback(get(assets::spa_fallback))
        .with_state(state)
}
