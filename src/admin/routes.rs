//! `/admin/api/*` 处理器。
//!
//! Phase 1 实现：health / overview / stats/usage / stats/models。
//! Phase 2 实现：upstreams* / models / sync。
//! Phase 3 实现：checkin* / credits*。
//! 日志查看：logs。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AdminState;

fn not_yet(what: &str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": {"message": format!("{what} 尚未实现"), "type": "not_implemented"}})),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct DaysQuery {
    #[serde(default = "default_days")]
    pub days: usize,
}

fn default_days() -> usize {
    7
}

/// `GET /admin/api/health`
pub async fn health(State(st): State<AdminState>) -> Response {
    Json(json!({
        "status": "ok",
        "data_dir": st.data_dir.to_string_lossy(),
        "upstreams": st.app.upstreams_snapshot().len(),
        "default_upstream": st.app.default_upstream,
    }))
    .into_response()
}

/// `GET /admin/api/overview?days=7`
pub async fn overview(State(st): State<AdminState>, Query(q): Query<DaysQuery>) -> Response {
    let days = q.days.clamp(1, 365);
    let mut body = st.app.metrics.overview(days);
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "upstreams".to_string(),
            json!(st.app.upstreams_snapshot().len()),
        );
        obj.insert(
            "default_upstream".to_string(),
            json!(st.app.default_upstream),
        );
    }
    Json(body).into_response()
}

/// `GET /admin/api/stats/usage?days=7`
pub async fn stats_usage(State(st): State<AdminState>, Query(q): Query<DaysQuery>) -> Response {
    Json(st.app.metrics.usage(q.days.clamp(1, 365))).into_response()
}

/// `GET /admin/api/stats/models?days=7`
pub async fn stats_models(State(st): State<AdminState>, Query(q): Query<DaysQuery>) -> Response {
    Json(st.app.metrics.model_latency(q.days.clamp(1, 365))).into_response()
}

// ---- Phase 2 占位 ----

/// `GET /admin/api/upstreams`
pub async fn list_upstreams(State(st): State<AdminState>) -> Response {
    let mut rows: Vec<Value> = Vec::new();
    for (name, up) in st.app.upstreams_snapshot() {
        let health = up.health_info().await;
        let model_count = up.models().await.len();
        rows.push(json!({
            "name": name,
            "model_count": model_count,
            "health": health,
        }));
    }
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(json!({"upstreams": rows})).into_response()
}

/// `GET /admin/api/upstreams/{name}`
pub async fn upstream_detail(
    State(st): State<AdminState>,
    Path(name): Path<String>,
) -> Response {
    match st.app.upstream(Some(&name)) {
        Some(up) => Json(json!({
            "name": name,
            "models": up.models().await,
            "health": up.health_info().await,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"message": format!("未配置的上游：{name}")}})),
        )
            .into_response(),
    }
}

/// `POST /admin/api/upstreams/reload`
pub async fn reload_upstreams(State(_st): State<AdminState>) -> Response {
    not_yet("上游重载")
}

/// `GET /admin/api/models`
pub async fn all_models(State(st): State<AdminState>) -> Response {
    let mut by_upstream: Vec<Value> = Vec::new();
    for (name, up) in st.app.upstreams_snapshot() {
        by_upstream.push(json!({
            "upstream": name,
            "models": up.models().await,
        }));
    }
    Json(json!({"by_upstream": by_upstream})).into_response()
}

/// `POST /admin/api/sync`
pub async fn sync_models(State(st): State<AdminState>) -> Response {
    let mut by_upstream: Vec<Value> = Vec::new();
    for (name, up) in st.app.upstreams_snapshot() {
        let models = up.models().await;
        by_upstream.push(json!({"upstream": name, "count": models.len(), "models": models}));
    }
    Json(json!({"ok": true, "synced_at": chrono::Local::now().to_rfc3339(), "by_upstream": by_upstream}))
        .into_response()
}

// ---- Phase 3 占位 ----

/// `POST /admin/api/checkin/trae`
pub async fn checkin_trae(State(_st): State<AdminState>) -> Response {
    not_yet("Trae 签到")
}

/// `GET /admin/api/checkin/results?days=30`
pub async fn checkin_results(State(_st): State<AdminState>, Query(_q): Query<DaysQuery>) -> Response {
    not_yet("签到记录")
}

/// `GET /admin/api/credits/trae`
pub async fn credits_trae(State(_st): State<AdminState>) -> Response {
    not_yet("Trae 积分")
}

/// `GET /admin/api/logs?lines=200`
#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_lines")]
    pub lines: usize,
}

fn default_lines() -> usize {
    200
}

pub async fn logs_tail(State(st): State<AdminState>, Query(q): Query<LogsQuery>) -> Response {
    let lines = q.lines.clamp(1, 5000);
    // 读取 data_dir 下最新的日志文件尾部。日志可能配置在别处，
    // 这里只扫 data_dir/logs 与日志目录；找不到就返回空。
    let log_dir = st.data_dir.join("logs");
    let mut newest: Option<std::path::PathBuf> = None;
    let mut newest_mtime = std::time::SystemTime::UNIX_EPOCH;
    if let Ok(rd) = std::fs::read_dir(&log_dir) {
        for e in rd.flatten() {
            if let Ok(md) = e.metadata() {
                if let Ok(mt) = md.modified() {
                    if mt > newest_mtime {
                        newest_mtime = mt;
                        newest = Some(e.path());
                    }
                }
            }
        }
    }
    let text = newest
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let tail: Vec<&str> = text
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Json(json!({"lines": tail, "file": newest.map(|p| p.to_string_lossy().to_string())}))
        .into_response()
}
