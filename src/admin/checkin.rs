//! Trae 一键签到。
//!
//! 端点（参考 TraeWorkAssistant 逆向实现，本机实测校准）：
//! - 预检：`POST https://api.trae.cn/trae/api/v2/ug/checkin_credits/status`
//! - 领奖：`POST https://api.trae.cn/trae/api/v2/ug/checkin_credits/claim`
//!
//! 与 llm_utils_chat 不同，这里鉴权用 `authorization: Cloud-IDE-JWT <jwt>`
//! （带前缀），user-agent 伪装 VSCode 客户端。
//!
//! 结果逐账号落 SQLite `checkin_records`（见 [`super::db`]），供趋势查询。

use serde_json::{json, Value};

use crate::upstream::trae::auth::{TraeAuthInfo, TraeCredential};

const API_HOST: &str = "https://api.trae.cn";
const CLAIM_URL: &str = "/trae/api/v2/ug/checkin_credits/claim";

/// 签到结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CheckinRecord {
    pub date: String,
    pub time: String,
    pub uid: String,
    /// success | already | failed
    pub status: String,
    pub message: String,
    #[serde(default)]
    pub credits: f64,
    #[serde(default)]
    pub delta: f64,
    /// 所属上游（旧记录无此字段时视为 trae）
    #[serde(default = "default_record_upstream")]
    pub upstream: String,
}

fn default_record_upstream() -> String {
    "trae".to_string()
}

/// 签到用请求头（与 chat 不同：Cloud-IDE-JWT 前缀 + VSCode UA）。
fn checkin_headers(info: &TraeAuthInfo) -> Vec<(String, String)> {
    let request_id = format!(
        "{}-{}-{}-{}-{}",
        crate::rand_hex(4),
        crate::rand_hex(2),
        crate::rand_hex(2),
        crate::rand_hex(2),
        crate::rand_hex(6)
    );
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "authorization".to_string(),
            format!("Cloud-IDE-JWT {}", info.token),
        ),
        (
            "user-agent".to_string(),
            "VSCode 1.107.1 (TRAE SOLO CN)".to_string(),
        ),
        ("x-market-user-id".to_string(), info.user_id.clone()),
        ("x-device-id".to_string(), info.device_id.clone()),
        ("x-request-id".to_string(), request_id),
    ]
}

/// 在响应信封里递归下钻找字段（沿 data/result/resp/response/info，≤8 层）。
pub fn find_payload_field(v: &Value, key: &str, depth: usize) -> Option<Value> {
    if depth > 8 {
        return None;
    }
    match v {
        Value::Object(m) => {
            if let Some(hit) = m.get(key) {
                if !hit.is_null() {
                    return Some(hit.clone());
                }
            }
            for k in ["data", "result", "resp", "response", "info"] {
                if let Some(next) = m.get(k) {
                    if let Some(hit) = find_payload_field(next, key, depth + 1) {
                        return Some(hit);
                    }
                }
            }
            // 兜底：遍历所有对象/数组值
            for val in m.values() {
                if val.is_object() || val.is_array() {
                    if let Some(hit) = find_payload_field(val, key, depth + 1) {
                        return Some(hit);
                    }
                }
            }
            None
        }
        Value::Array(items) => {
            for i in items {
                if let Some(hit) = find_payload_field(i, key, depth + 1) {
                    return Some(hit);
                }
            }
            None
        }
        _ => None,
    }
}

fn as_f64(v: Option<Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// 单账号签到（领奖）。
pub async fn checkin_one(info: &TraeAuthInfo) -> CheckinRecord {
    let now = chrono::Local::now();
    let mut record = CheckinRecord {
        date: now.format("%Y-%m-%d").to_string(),
        time: now.format("%H:%M:%S").to_string(),
        uid: info.user_id.clone(),
        status: "failed".to_string(),
        message: String::new(),
        credits: 0.0,
        delta: 0.0,
        upstream: "trae".to_string(),
    };

    let client = reqwest::Client::new();
    let mut req = client.post(format!("{API_HOST}{CLAIM_URL}"));
    for (k, v) in checkin_headers(info) {
        req = req.header(k, v);
    }
    let resp = match req
        .json(&json!({}))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            record.message = format!("网络错误: {e}");
            return record;
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        record.message = format!(
            "HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        );
        return record;
    }
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        record.message = format!(
            "响应非 JSON: {:?}",
            text.chars().take(200).collect::<String>()
        );
        return record;
    };

    let code = v.get("code").and_then(|c| c.as_i64());
    let message = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    record.credits = as_f64(find_payload_field(&v, "credits", 0));
    record.delta = as_f64(find_payload_field(&v, "delta", 0))
        .max(as_f64(find_payload_field(&v, "credits", 0)));

    match code {
        Some(0) => {
            record.status = "success".to_string();
            record.message = if message.is_empty() {
                "签到成功".to_string()
            } else {
                message
            };
        }
        // 业务码语义（参考实现）：1005=套餐限制，4008=配额，1001=登录失效；
        // 重复签到各家表述不一，按 message 关键字归类
        Some(c) => {
            let lower = message.to_lowercase();
            if lower.contains("already")
                || lower.contains("重复")
                || lower.contains("已签")
                || lower.contains("已经签")
            {
                record.status = "already".to_string();
                record.message = if message.is_empty() {
                    "今日已签到".to_string()
                } else {
                    message
                };
            } else if c == 1001 {
                record.message = format!("登录失效({c}): {message}");
            } else {
                record.message = format!("code={c}: {message}");
            }
        }
        None => {
            // 无 code 字段：按 checked_in 布尔判断
            match find_payload_field(&v, "checked_in", 0) {
                Some(Value::Bool(true)) => {
                    record.status = "already".to_string();
                    record.message = "今日已签到".to_string();
                }
                _ => {
                    record.message = format!(
                        "无法识别的响应: {:?}",
                        serde_json::to_string(&v)
                            .unwrap_or_default()
                            .chars()
                            .take(200)
                            .collect::<String>()
                    );
                }
            }
        }
    }
    record
}

/// 找到可用的 Trae 凭据（尊重注册表是否启用 trae）。
pub async fn trae_info(st: &super::AdminState) -> Result<TraeAuthInfo, String> {
    if st.app.upstream(Some("trae")).is_none() {
        return Err("Trae 上游未启用或未找到登录态（storage.json）".to_string());
    }
    let explicit = st
        .app
        .registry_base
        .trae_storage_json
        .clone()
        .map(std::path::PathBuf::from);
    let path = explicit
        .or_else(crate::upstream::trae::auth::find_storage_json)
        .ok_or("未找到 Trae storage.json")?;
    TraeCredential::new(path).info().await
}

/// 执行一键签到并落库。
pub async fn run_checkin(st: &super::AdminState) -> Result<Value, String> {
    let info = trae_info(st).await?;
    let record = checkin_one(&info).await;
    save_record(st, &record);
    Ok(json!({
        "ok": record.status != "failed",
        "saved": true,
        "result": record,
    }))
}

/// 把一条签到结果写入 DB 队列（非阻塞；落库由后台写入任务串行完成）。
pub fn save_record(st: &super::AdminState, record: &CheckinRecord) {
    st.recorder.checkin(super::db::CheckinEvent {
        ts_ms: chrono::Local::now().timestamp_millis(),
        date: record.date.clone(),
        time: record.time.clone(),
        uid: record.uid.clone(),
        status: record.status.clone(),
        message: record.message.clone(),
        credits: record.credits,
        delta: record.delta,
        upstream: record.upstream.clone(),
    });
}

/// 签到记录与按日趋势（按上游过滤；旧记录无 upstream 字段视为 trae）。
pub fn results(st: &super::AdminState, days: usize, upstream: &str) -> Value {
    // 该函数是同步的（被 axum handler 直接调用），因此用阻塞方式取数：
    // 在专用 runtime 上 block_on，避免把 handler 改成 async 的连锁改动。
    let cutoff = (chrono::Local::now() - chrono::Duration::days(days as i64 - 1))
        .format("%Y-%m-%d")
        .to_string();
    let up = upstream.to_string();
    let rows = block_on(st.db.checkin_rows(&cutoff))
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let recent: Vec<CheckinRecord> = rows
        .into_iter()
        .filter(|r| r.upstream == up || (up == "trae" && r.upstream.is_empty()))
        .map(|r| CheckinRecord {
            date: r.date,
            time: r.time,
            uid: r.uid,
            status: r.status,
            message: r.message,
            credits: r.credits,
            delta: r.delta,
            upstream: r.upstream,
        })
        .collect();
    // 按日聚合趋势
    let mut trend: std::collections::BTreeMap<String, (u64, u64, u64)> =
        std::collections::BTreeMap::new();
    for r in &recent {
        let e = trend.entry(r.date.clone()).or_default();
        match r.status.as_str() {
            "success" => e.0 += 1,
            "already" => e.1 += 1,
            _ => e.2 += 1,
        }
    }
    let trend: Vec<Value> = trend
        .into_iter()
        .map(|(date, (ok, already, failed))| {
            json!({"date": date, "ok": ok, "already": already, "failed": failed})
        })
        .collect();
    json!({
        "records": recent.iter().rev().take(100).collect::<Vec<_>>(),
        "trend": trend,
    })
}

/// 在当前线程的公共 runtime 上阻塞执行一个 future（供同步 handler 调用 async DB）。
///
/// axum handler 运行于 tokio runtime 上下文，此处复用该 runtime 的 handle；
/// 若不在 runtime 上下文（单测），则临时建一个。
fn block_on<F: std::future::Future>(fut: F) -> Result<F::Output, String> {
    use tokio::runtime::{Handle, Runtime};
    if let Ok(h) = Handle::try_current() {
        // 已在运行时内：用 block_in_place 防止阻塞 worker
        return Ok(tokio::task::block_in_place(|| h.block_on(fut)));
    }
    let rt = Runtime::new().map_err(|e| format!("创建 runtime 失败: {e}"))?;
    Ok(rt.block_on(fut))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_info() -> TraeAuthInfo {
        TraeAuthInfo {
            token: "jwt-x".to_string(),
            user_id: "u1".to_string(),
            device_id: "d1".to_string(),
            machine_id: "m1".to_string(),
            expires_at_ms: 0,
            host_variant: "Trae CN".to_string(),
            storage_path: std::path::PathBuf::from("/fake"),
        }
    }

    #[test]
    fn test_find_payload_field_digs_through_envelopes() {
        let v = json!({"code": 0, "data": {"result": {"credits": 15.5}}});
        assert_eq!(find_payload_field(&v, "credits", 0), Some(json!(15.5)));
        let v = json!({"resp": {"response": {"info": {"checked_in": true}}}});
        assert_eq!(find_payload_field(&v, "checked_in", 0), Some(json!(true)));
    }

    #[test]
    fn test_find_payload_field_miss() {
        let v = json!({"code": 0, "data": {}});
        assert_eq!(find_payload_field(&v, "credits", 0), None);
    }

    #[test]
    fn test_as_f64_variants() {
        assert_eq!(as_f64(Some(json!(15))), 15.0);
        assert_eq!(as_f64(Some(json!("12.5"))), 12.5);
        assert_eq!(as_f64(None), 0.0);
    }

    #[test]
    fn test_checkin_headers_use_cloudide_jwt() {
        let info = test_info();
        let headers = checkin_headers(&info);
        let map: std::collections::HashMap<_, _> = headers.iter().cloned().collect();
        assert_eq!(map.get("authorization").unwrap(), "Cloud-IDE-JWT jwt-x");
        assert_eq!(map.get("x-market-user-id").unwrap(), "u1");
        assert_eq!(
            map.get("user-agent").unwrap(),
            "VSCode 1.107.1 (TRAE SOLO CN)"
        );
    }
}
