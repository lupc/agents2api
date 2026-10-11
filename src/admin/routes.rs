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

/// `<data_dir>/model_catalog.json` 路径（各上游详细目录快照）。
fn catalog_snapshot_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("model_catalog.json")
}

/// `GET /admin/api/models` — 跨上游聚合模型列表（含最近一次同步的详细目录快照）。
pub async fn all_models(State(st): State<AdminState>) -> Response {
    let mut by_upstream: Vec<Value> = Vec::new();
    for (name, up) in st.app.upstreams_snapshot() {
        by_upstream.push(json!({
            "upstream": name,
            "models": up.models().await,
        }));
    }
    let mut snapshot: Option<Value> = std::fs::read_to_string(catalog_snapshot_path(&st.data_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    // 旧命名迁移：快照里的 codebuddy / workbuddy 键合并为 buddy
    if let Some(snap) = &mut snapshot {
        if let Some(by) = snap.get_mut("by_upstream") {
            migrate_snapshot_keys(by);
        }
    }
    Json(json!({"by_upstream": by_upstream, "snapshot": snapshot})).into_response()
}

/// 目录快照旧命名迁移：by_upstream 里的 codebuddy / workbuddy 键合并为 buddy
/// （二者原是同一上游的两个别名，取有 models 的那个；buddy 已存在则优先 buddy）。
/// 原地修改。
fn migrate_snapshot_keys(by_upstream: &mut Value) {
    let Some(obj) = by_upstream.as_object_mut() else { return };
    let mut legacy_entry: Option<Value> = None;
    for old in ["codebuddy", "workbuddy"] {
        if let Some(e) = obj.remove(old) {
            // 优先取有 models 的条目
            let has_models = e
                .get("models")
                .and_then(|m| m.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            legacy_entry = match legacy_entry.take() {
                Some(prev) if !has_models => Some(prev),
                _ => Some(e),
            };
        }
    }
    if let Some(e) = legacy_entry {
        obj.entry("buddy".to_string())
            .or_insert(e);
    }
}

/// 同步核心逻辑（HTTP 处理器与每日定时任务共用）。
pub async fn sync_models_inner(st: &AdminState) -> Value {
    let synced_at = chrono::Local::now().to_rfc3339();
    let mut by_upstream: serde_json::Map<String, Value> = serde_json::Map::new();
    for (name, up) in st.app.upstreams_snapshot() {
        match up.sync_catalog().await {
            Ok(models) => {
                // 同步成功后即刻写入内存快照（models() 立即消费）；
                // 同时落盘（重启时 load_*_catalog_snapshot 恢复）
                if name == "buddy" {
                    crate::upstream::codebuddy_catalog::set_catalog_snapshot(models.clone());
                    if let Err(e) = crate::upstream::codebuddy_catalog::save_catalog_snapshot(
                        &st.data_dir,
                        &models,
                        &synced_at,
                    ) {
                        crate::config::log(&format!("⚠️ {} 目录落盘失败: {e}", name));
                    }
                } else if name == "trae" {
                    crate::upstream::trae::catalog::set_catalog_snapshot(models.clone());
                    if let Err(e) = crate::upstream::trae::catalog::save_catalog_snapshot(
                        &st.data_dir,
                        &models,
                        &synced_at,
                    ) {
                        crate::config::log(&format!("⚠️ {} 目录落盘失败: {e}", name));
                    }
                }
                crate::config::log(&format!(
                    "🔄 {} 模型目录同步成功：{} 个模型",
                    name,
                    models.len()
                ));
                by_upstream.insert(
                    name,
                    json!({"count": models.len(), "models": models, "error": null}),
                );
            }
            Err(e) => {
                crate::config::log(&format!("⚠️ {} 模型目录同步失败: {e}", name));
                by_upstream.insert(name, json!({"error": e}));
            }
        }
    }
    let snapshot = json!({"synced_at": synced_at, "by_upstream": by_upstream});
    let path = catalog_snapshot_path(&st.data_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    let write_ok = serde_json::to_string_pretty(&snapshot)
        .ok()
        .and_then(|t| std::fs::write(&tmp, t).ok())
        .and_then(|_| std::fs::rename(&tmp, &path).ok())
        .is_some();
    json!({
        "ok": write_ok,
        "synced_at": synced_at,
        "by_upstream": snapshot["by_upstream"],
    })
}

/// `POST /admin/api/sync` — 触发各上游模型目录同步（`sync_catalog`）。
///
/// 每上游独立成败：支持的（codebuddy/trae）拉远程目录并落盘 + 写内存快照
/// （models() 即刻消费）；不支持的记录原因。快照结构：
/// ```json
/// {"synced_at": "...", "by_upstream": {"codebuddy": {"models": [ModelInfo...], "error": null}}}
/// ```
pub async fn sync_models(State(st): State<AdminState>) -> Response {
    Json(sync_models_inner(&st).await).into_response()
}

// ---- 后台设置（每日定时同步）----

/// `<data_dir>/admin_settings.json`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdminSettings {
    /// 每日定时同步开关。
    #[serde(default)]
    pub daily_sync_enabled: bool,
    /// 每日同步时间（本地 HH:MM）。
    #[serde(default = "default_sync_time")]
    pub daily_sync_time: String,
    /// 上次自动同步日期（防止一天内重复触发）。
    #[serde(default)]
    pub last_sync_date: String,
}

fn default_sync_time() -> String {
    "05:45".to_string()
}

impl Default for AdminSettings {
    fn default() -> Self {
        Self {
            daily_sync_enabled: false,
            daily_sync_time: default_sync_time(),
            last_sync_date: String::new(),
        }
    }
}

impl AdminSettings {
    pub fn load(data_dir: &std::path::Path) -> Self {
        std::fs::read_to_string(data_dir.join("admin_settings.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &std::path::Path) -> Result<(), String> {
        let path = data_dir.join("admin_settings.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }
}

/// `GET /admin/api/settings`
pub async fn get_settings(State(st): State<AdminState>) -> Response {
    Json(AdminSettings::load(&st.data_dir)).into_response()
}

/// `PUT /admin/api/settings`
pub async fn put_settings(State(st): State<AdminState>, Json(body): Json<Value>) -> Response {
    let mut s = AdminSettings::load(&st.data_dir);
    if let Some(b) = body.get("daily_sync_enabled").and_then(|v| v.as_bool()) {
        s.daily_sync_enabled = b;
    }
    if let Some(t) = body.get("daily_sync_time").and_then(|v| v.as_str()) {
        // 宽松校验 HH:MM
        let ok = t.len() == 5
            && t.as_bytes()[2] == b':'
            && t.chars().enumerate().all(|(i, c)| i == 2 || c.is_ascii_digit());
        if !ok {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": "时间格式应为 HH:MM"}})),
            )
                .into_response();
        }
        s.daily_sync_time = t.to_string();
    }
    if let Err(e) = s.save(&st.data_dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": format!("保存设置失败: {e}")}})),
        )
            .into_response();
    }
    Json(s).into_response()
}

// ---- 签到 / 积分（trae + buddy）----

/// `POST /admin/api/checkin/trae` — Trae 一键签到。
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

/// `POST /admin/api/checkin/buddy` — Buddy 一键签到。
pub async fn checkin_buddy(State(st): State<AdminState>) -> Response {
    match super::buddy_billing::run_checkin(&st).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"message": e}})),
        )
            .into_response(),
    }
}

/// `GET /admin/api/checkin/results?days=30&upstream=trae`
#[derive(Deserialize)]
pub struct ResultsQuery {
    #[serde(default = "default_days")]
    pub days: usize,
    #[serde(default = "default_results_upstream")]
    pub upstream: String,
}

fn default_results_upstream() -> String {
    "trae".to_string()
}

pub async fn checkin_results(
    State(st): State<AdminState>,
    Query(q): Query<ResultsQuery>,
) -> Response {
    Json(
        super::checkin::results(&st, q.days.clamp(1, 365), &q.upstream),
    )
    .into_response()
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

/// `GET /admin/api/credits/buddy`
pub async fn credits_buddy(State(st): State<AdminState>) -> Response {
    match super::buddy_billing::fetch_credits(&st).await {
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
    // 优先读 --log-dir 指定的滚动日志目录（递归找最新文件），退回 data_dir/logs
    let mut newest: Option<std::path::PathBuf> = None;
    let mut newest_mtime = std::time::SystemTime::UNIX_EPOCH;
    fn search(dir: &std::path::Path, newest: &mut Option<std::path::PathBuf>, newest_mtime: &mut std::time::SystemTime) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                // 日期子目录，深入一层
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for se in sub.flatten() {
                        if let Ok(md) = se.metadata() {
                            if let Ok(mt) = md.modified() {
                                if mt > *newest_mtime {
                                    *newest_mtime = mt;
                                    *newest = Some(se.path());
                                }
                            }
                        }
                    }
                }
            } else if let Ok(md) = e.metadata() {
                if let Ok(mt) = md.modified() {
                    if mt > *newest_mtime {
                        *newest_mtime = mt;
                        *newest = Some(p);
                    }
                }
            }
        }
    }

    if let Some(ld) = &st.app.log_dir {
        search(ld, &mut newest, &mut newest_mtime);
    }
    if newest.is_none() {
        search(&st.data_dir.join("logs"), &mut newest, &mut newest_mtime);
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
