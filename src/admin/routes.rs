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
use super::config_store;

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
        "default_upstream": st.app.default_upstream(),
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
            json!(st.app.default_upstream()),
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

/// `POST /admin/api/upstreams/reload` — 按配置文件重建上游注册表。
pub async fn reload_upstreams(State(st): State<AdminState>) -> Response {
    let cfg = config_store::UpstreamsConfig::load(&st.data_dir);
    let registry = config_store::build_registry(&st.app.registry_base, &[], &cfg);
    let count = registry.len();
    let default = registry.default_name();
    st.app.upstreams.replace(registry);
    crate::config::log(&format!("🔄 上游注册表已重载：{count} 个上游，默认 {default}"));
    Json(json!({
        "ok": true,
        "upstream_count": count,
        "default_upstream": default,
        "upstreams": st.app.upstreams_snapshot().iter().map(|(k, _)| k).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// `GET /admin/api/config` — 当前可编辑配置（文件覆盖 + CLI 兜底合并视图）。
pub async fn get_config(State(st): State<AdminState>) -> Response {
    let cfg = config_store::UpstreamsConfig::load(&st.data_dir);
    let mut view = config_store::config_view(&cfg, &st.app.registry_base);
    if let Some(obj) = view.as_object_mut() {
        obj.insert(
            "cli_default_upstream".to_string(),
            json!(st.app.cli_default_upstream),
        );
    }
    Json(view).into_response()
}

/// `PUT /admin/api/config` — 保存配置并立即重建注册表。
pub async fn put_config(State(st): State<AdminState>, Json(body): Json<Value>) -> Response {
    let mut cfg = config_store::UpstreamsConfig::load(&st.data_dir);
    if let Some(d) = body.get("default_upstream").and_then(|v| v.as_str()) {
        cfg.default_upstream = Some(d.to_string());
    }
    if let Some(upstreams) = body.get("upstreams").and_then(|v| v.as_object()) {
        for (name, val) in upstreams {
            let entry = cfg.upstreams.entry(name.clone()).or_default();
            if let Some(b) = val.get("enabled").and_then(|v| v.as_bool()) {
                entry.enabled = b;
            }
            if let Some(b) = val.get("backend").and_then(|v| v.as_str()) {
                entry.backend = if b.is_empty() { None } else { Some(b.to_string()) };
            }
            if let Some(h) = val.get("chat_host").and_then(|v| v.as_str()) {
                entry.chat_host = if h.is_empty() { None } else { Some(h.to_string()) };
            }
        }
    }
    if let Err(e) = config_store::UpstreamsConfig::save(&st.data_dir, &cfg) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": format!("保存配置失败: {e}")}})),
        )
            .into_response();
    }
    // 保存后立即重建
    let registry = config_store::build_registry(&st.app.registry_base, &[], &cfg);
    let count = registry.len();
    let default = registry.default_name();
    st.app.upstreams.replace(registry);
    crate::config::log(&format!("🔄 配置已保存并重载：{count} 个上游，默认 {default}"));
    Json(json!({"ok": true, "upstream_count": count, "default_upstream": default}))
        .into_response()
}

/// `<data_dir>/models.json` 路径。
fn models_snapshot_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("models.json")
}

/// `GET /admin/api/models` — 跨上游聚合模型列表（有同步快照则带出）。
pub async fn all_models(State(st): State<AdminState>) -> Response {
    let mut by_upstream: Vec<Value> = Vec::new();
    for (name, up) in st.app.upstreams_snapshot() {
        by_upstream.push(json!({
            "upstream": name,
            "models": up.models().await,
        }));
    }
    // 附带最近一次同步快照（若有）
    let snapshot: Option<Value> = std::fs::read_to_string(models_snapshot_path(&st.data_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    Json(json!({"by_upstream": by_upstream, "snapshot": snapshot})).into_response()
}

/// `POST /admin/api/sync` — 触发各上游模型同步（`sync_models`），快照落盘。
pub async fn sync_models(State(st): State<AdminState>) -> Response {
    let synced_at = chrono::Local::now().to_rfc3339();
    let mut by_upstream: Vec<Value> = Vec::new();
    for (name, up) in st.app.upstreams_snapshot() {
        let models = up.sync_models().await;
        by_upstream.push(json!({
            "upstream": name,
            "count": models.len(),
            "models": models,
            "synced_at": synced_at,
        }));
    }
    let snapshot = json!({"synced_at": synced_at, "by_upstream": by_upstream});
    let path = models_snapshot_path(&st.data_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    let write_ok = serde_json::to_string_pretty(&snapshot)
        .ok()
        .and_then(|t| std::fs::write(&tmp, t).ok())
        .and_then(|_| std::fs::rename(&tmp, &path).ok())
        .is_some();
    Json(json!({
        "ok": write_ok,
        "synced_at": synced_at,
        "by_upstream": by_upstream,
    }))
    .into_response()
}

// ---- Phase 3：Trae 签到 / 积分 ----

/// `POST /admin/api/checkin/trae` — 一键签到。
pub async fn checkin_trae(State(st): State<AdminState>) -> Response {
    match super::checkin::run_checkin(&st).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": e}})),
        )
            .into_response(),
    }
}

/// `GET /admin/api/checkin/results?days=30`
pub async fn checkin_results(
    State(st): State<AdminState>,
    Query(q): Query<DaysQuery>,
) -> Response {
    Json(super::checkin::results(&st, q.days.clamp(1, 365))).into_response()
}

/// `GET /admin/api/credits/trae`
pub async fn credits_trae(State(st): State<AdminState>) -> Response {
    match super::credits::fetch(&st).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": e}})),
        )
            .into_response(),
    }
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
