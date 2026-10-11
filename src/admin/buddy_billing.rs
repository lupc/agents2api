//! Buddy（copilot.tencent.com / codebuddy.cn 计费域）签到与积分。
//!
//! 端点移植自参考实现 TraeWorkAssistant 的 `wb_checkin.rs` / `wb_credits.rs`
//! （2026-09 抓包实证；计费域 `www.codebuddy.cn` 与 auth 文件 domain 字段一致）：
//!
//! - 签到预检：`POST {base}/v2/billing/meter/checkin-activity-status`
//!   （旧路径 `checkin-status` 兜底），dug `today_checked_in/...`
//! - 签到执行：`POST {base}/v2/billing/meter/daily-checkin`
//!   success = HTTP 2xx 或 code∈{0,200}；already = code 10001 或 message
//!   含「已签到/repeat/already」；奖励数额从常见键 + 深挖兜底
//! - 积分：summary → paid/free 带 PackageCodes 回查到期 → 旧接口回退
//! - 主域名网络不可达 → 备用域名（www.workbuddy.ai）重试一轮
//!
//! 鉴权复用凭据头（Authorization Bearer / X-User-Id / X-Domain）+ web 平台标识。

use serde_json::{json, Value};

use super::checkin::CheckinRecord;
use crate::auth::CredentialManager;

const BILLING_BASE_CN: &str = "https://www.codebuddy.cn";
const BILLING_BASE_ALT: &str = "https://www.workbuddy.ai";

/// Buddy 计费请求头：复用凭据头（Authorization Bearer / X-User-Id / X-Domain），
/// 补 web 平台标识（参考实现 build_auth_headers(creds, true)）。
async fn billing_headers() -> Result<Vec<(String, String)>, String> {
    let path = crate::auth::find_auth_file()
        .ok_or("未找到 Buddy 登录态文件（CodeBuddyExtension auth）")?;
    let cred = CredentialManager::new(path);
    let mut headers = cred.get_headers().await?;
    headers.push(("X-Client-Platform".to_string(), "web".to_string()));
    Ok(headers)
}

async fn post_json(
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<(u16, Option<Value>, String), String> {
    let client = reqwest::Client::new();
    let mut req = client.post(url).json(body);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("网络不可达: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| format!("读取失败: {e}"))?;
    let parsed = serde_json::from_str(&text).ok();
    Ok((status, parsed, text))
}

// ── 数值/布尔解析辅助（对齐参考实现语义）────────────────────────────

fn num_or_none(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn py_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(true, |f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(_)) => true,
    }
}

// ── 签到 ────────────────────────────────────────────────────────────

/// 签到预检：返回 (今日已签?, 接口可达)。新路径失败试旧路径。
async fn checkin_status(base: &str, headers: &[(String, String)]) -> (Option<bool>, bool) {
    for path in [
        "/v2/billing/meter/checkin-activity-status",
        "/v2/billing/meter/checkin-status",
    ] {
        let Ok((status, body, _)) = post_json(&format!("{base}{path}"), headers, &json!({})).await else {
            continue;
        };
        if status == 401 {
            return (None, false);
        }
        if status == 200 {
            if let Some(b) = body.filter(|b| b.is_object()) {
                let mut v = super::checkin::find_payload_field(&b, "today_checked_in", 0)
                    .or_else(|| super::checkin::find_payload_field(&b, "todayCheckedIn", 0))
                    .or_else(|| super::checkin::find_payload_field(&b, "checked_in", 0))
                    .or_else(|| super::checkin::find_payload_field(&b, "checkedIn", 0));
                if v.is_none() {
                    // 部分响应以 0/1 表达
                    v = super::checkin::find_payload_field(&b, "checked", 0)
                        .or_else(|| super::checkin::find_payload_field(&b, "is_checked", 0));
                }
                if let Some(v) = v {
                    return (Some(py_truthy(Some(&v))), true);
                }
                return (None, true); // 200 但字段缺失：未知但不失败
            }
        }
        // 其它状态码尝试旧路径
    }
    (None, false)
}

/// 奖励数额：常见键 → 深挖（对齐参考实现 reward 键表）。
fn reward_from(body: &Value) -> Option<f64> {
    const KEYS: &[&str] = &[
        "reward", "credits", "points", "amount", "integral", "score", "bonus",
        "reward_amount", "add_integral", "addCredits", "earned",
    ];
    for k in KEYS {
        if let Some(v) = super::checkin::find_payload_field(body, k, 0) {
            if let Some(n) = num_or_none(Some(&v)) {
                return Some(n);
            }
        }
    }
    None
}

/// 执行签到：预检（已签跳过）→ daily-checkin（主域名网络失败切备用域名）。
async fn checkin_once(headers: &[(String, String)]) -> CheckinRecord {
    let now = chrono::Local::now();
    let mut record = CheckinRecord {
        date: now.format("%Y-%m-%d").to_string(),
        time: now.format("%H:%M:%S").to_string(),
        uid: "buddy".to_string(),
        status: "failed".to_string(),
        message: String::new(),
        credits: 0.0,
        delta: 0.0,
        upstream: "buddy".to_string(),
    };

    // 预检：已签则跳过（预检不可达不阻断——执行时容错）
    let (checked, reachable) = checkin_status(BILLING_BASE_CN, headers).await;
    if reachable && checked == Some(true) {
        record.status = "already".to_string();
        record.message = "今日已签到（预检）".to_string();
        return record;
    }

    // 执行：主域名 → 备用域名（仅网络不可达时）
    let mut attempt = post_json(
        &format!("{BILLING_BASE_CN}/v2/billing/meter/daily-checkin"),
        headers,
        &json!({}),
    )
    .await;
    if let Ok((0, _, ref raw)) = attempt {
        if raw.starts_with("网络不可达") {
            attempt = post_json(
                &format!("{BILLING_BASE_ALT}/v2/billing/meter/daily-checkin"),
                headers,
                &json!({}),
            )
            .await;
        }
    }
    let (status, body, raw) = match attempt {
        Ok(v) => v,
        Err(e) => {
            record.message = e;
            return record;
        }
    };
    if status == 401 {
        record.message = "登录态失效（401）".to_string();
        return record;
    }
    let Some(b) = body.filter(|b| b.is_object()) else {
        let head: String = raw.chars().take(120).collect();
        record.message = if head.is_empty() {
            format!("HTTP {status}")
        } else {
            head
        };
        return record;
    };
    let code = b
        .get("code")
        .and_then(Value::as_i64)
        .or_else(|| super::checkin::find_payload_field(&b, "code", 0).and_then(|v| v.as_i64()));
    let message = super::checkin::find_payload_field(&b, "message", 0)
        .or_else(|| super::checkin::find_payload_field(&b, "msg", 0))
        .and_then(|v| match &v {
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        })
        .unwrap_or_default();

    if (200..=201).contains(&status) || code.is_some_and(|c| c == 0 || c == 200) {
        record.status = "success".to_string();
        record.message = "签到成功".to_string();
        if let Some(reward) = reward_from(&b) {
            record.credits = reward;
            record.delta = reward;
        }
        return record;
    }
    // 已签容错：code 10001 / message 含「已签到/repeat/already」
    let msg_low = message.to_ascii_lowercase();
    if code == Some(10001)
        || ["已签到", "repeat", "already"].iter().any(|k| msg_low.contains(k))
    {
        record.status = "already".to_string();
        record.message = "今日已签到".to_string();
        return record;
    }
    let head: String = raw.chars().take(120).collect();
    record.message = if message.is_empty() {
        if head.is_empty() {
            format!("HTTP {status}")
        } else {
            head
        }
    } else {
        format!("{message}（code={}）", code.map(|c| c.to_string()).unwrap_or_else(|| "None".into()))
    };
    record
}

/// Buddy 一键签到入口（落盘复用 checkin.json，记录带 upstream=buddy）。
pub async fn run_checkin(st: &super::AdminState) -> Result<Value, String> {
    if st.app.upstream(Some("buddy")).is_none() {
        return Err("Buddy 上游未启用或未找到登录态（CodeBuddyExtension auth）".to_string());
    }
    let headers = billing_headers().await?;
    let mut record = checkin_once(&headers).await;
    record.upstream = "buddy".to_string();

    // 追加落盘（与 trae 共用 checkin.json；按 upstream 字段区分）
    let path = st.data_dir.join("checkin.json");
    let mut records: Vec<CheckinRecord> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    records.push(record.clone());
    let cutoff = (chrono::Local::now() - chrono::Duration::days(90))
        .format("%Y-%m-%d")
        .to_string();
    records.retain(|r| r.date >= cutoff);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    let ok = serde_json::to_string_pretty(&records)
        .ok()
        .and_then(|t| std::fs::write(&tmp, t).ok())
        .and_then(|_| std::fs::rename(&tmp, &path).ok())
        .is_some();

    Ok(json!({
        "ok": record.status != "failed",
        "saved": ok,
        "result": record,
    }))
}

// ── 积分 ────────────────────────────────────────────────────────────

/// 容量字段键表（含这些键的对象视为积分包/容量条目）。
const CAPACITY_KEYS: &[&str] = &[
    "CycleCapacitySizePrecise",
    "CycleRemainCapacity",
    "CycleCapacityRemainPrecise",
    "CycleCapacityRemain",
    "RemainingCapacity",
    "Remaining",
    "CycleTotalCapacity",
    "CycleCapacitySize",
    "CapacitySize",
    "TotalCapacity",
    "UsedCapacity",
    "CycleUsedCapacity",
];

fn dig<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k))
}

fn walk_capable_dicts<'a>(v: &'a Value, depth: usize, out: &mut Vec<&'a Value>) {
    if depth > 8 {
        return;
    }
    match v {
        Value::Object(map) => {
            if CAPACITY_KEYS.iter().any(|k| map.contains_key(*k)) {
                out.push(v);
            }
            for child in map.values() {
                walk_capable_dicts(child, depth + 1, out);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                walk_capable_dicts(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn to_ts(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    match v {
        Value::Number(n) => {
            let f = n.as_f64()?;
            // 秒 or 毫秒
            Some(if f > 1e12 { f as i64 / 1000 } else { f as i64 })
        }
        Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<f64>() {
                return Some(if n > 1e12 { n as i64 / 1000 } else { n as i64 });
            }
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.timestamp())
                .or_else(|| {
                    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                        .ok()
                        .map(|d| d.and_utc().timestamp())
                })
        }
        _ => None,
    }
}

pub(crate) fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// unix 秒 → 本地日期串（供 trae 积分包复用）。
pub(crate) fn fmt_ts_local(ts: i64) -> Option<String> {
    Some(fmt_ts(ts))
}

/// 容量条目 → 积分包明细（对齐参考实现 packages_from 字段链）。
fn packages_from(body: &Value) -> Vec<Value> {
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let mut out = vec![];
    for item in found {
        let total = num_or_none(dig(
            item,
            &["CycleCapacitySizePrecise", "CycleTotalCapacity", "CycleCapacitySize", "CapacitySize", "TotalCapacity"],
        ));
        let remaining = num_or_none(dig(
            item,
            &["CycleRemainCapacity", "CycleCapacityRemainPrecise", "CycleCapacityRemain", "RemainingCapacity", "Remaining"],
        ));
        if remaining.is_none() && total.is_none() {
            continue;
        }
        let name = dig(item, &["PackageName", "packageName", "Name", "name", "ProductCode", "description"])
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "积分包".to_string());
        let code = dig(item, &["PackageCode", "packageCode"])
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .filter(|s| !s.is_empty());
        let end_raw = dig(
            item,
            &[
                "DeductionEndTime", "deductionEndTime", "PackageEndTime",
                "packageEndTime", "EndTime", "endTime", "expireTime",
                "expireAt", "ExpireTime", "expire_time", "expiredAt", "ExpiredTime",
            ],
        );
        let end_ts = to_ts(end_raw);
        let used = num_or_none(dig(item, &["CycleUsedCapacity", "UsedCapacity", "used", "Used"]));
        out.push(json!({
            "name": name,
            "code": code,
            "granted": total.unwrap_or(0.0),
            "remaining": remaining.unwrap_or(0.0),
            "total": total.unwrap_or(0.0),
            "used": used.unwrap_or(0.0),
            "end_time": end_ts.map(fmt_ts),
        }));
    }
    out
}

/// summary 响应 → 余额：顶层键 → data.Packages[] 各包剩余求和 → 总容量兜底。
fn balance_from_summary(body: &Value) -> Option<f64> {
    if let Some(t) = num_or_none(dig(
        body,
        &["RemainingCapacity", "remaining", "TotalRemaining", "Balance", "balance"],
    )) {
        return Some(t);
    }
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let mut s = 0.0;
    let mut has = false;
    for item in &found {
        if let Some(v) = num_or_none(dig(
            item,
            &["CycleRemainCapacity", "RemainingCapacity", "remaining"],
        )) {
            s += v;
            has = true;
        }
    }
    if has {
        return Some(s);
    }
    num_or_none(dig(body, &["CycleCapacitySizePrecise", "CycleTotalCapacity", "CapacitySize"]))
}

/// summary 响应 → PackageCode 列表（去重，保序）。
fn package_codes_from(body: &Value) -> Vec<String> {
    let mut found = vec![];
    walk_capable_dicts(body, 0, &mut found);
    let mut codes: Vec<String> = vec![];
    for item in found {
        if let Some(c) = dig(item, &["PackageCode", "packageCode"]).and_then(Value::as_str) {
            if !c.is_empty() && !codes.iter().any(|x| x == c) {
                codes.push(c.to_string());
            }
        }
    }
    codes
}

/// 旧接口参数（回退路径）。
fn old_body() -> Value {
    json!({
        "ProductCode": "p_tcaca",
        "Status": [0, 3],
        "PackageEndTimeRange": {
            "StartTime": "2000-01-01T00:00:00Z",
            "EndTime": "2099-12-31T23:59:59Z"
        }
    })
}

/// 单轮取数 → (积分包明细, 余额, saw_auth, net_down)。
/// summary（余额 + PackageCode）→ paid/free 带 PackageCodes 回查到期。
async fn fetch_round(
    base: &str,
    headers: &[(String, String)],
) -> (Vec<Value>, Option<f64>, bool, bool) {
    let Ok((status, Some(body), _)) = post_json(
        &format!("{base}/billing/meter/get-user-resource-summary"),
        headers,
        &json!({}),
    )
    .await
    else {
        return (vec![], None, false, true);
    };
    if status == 401 {
        return (vec![], None, true, false);
    }
    if status != 200 {
        return (vec![], None, false, false);
    }

    let balance = balance_from_summary(&body);
    let codes = package_codes_from(&body);

    let mut enriched: Vec<Value> = vec![];
    let mut enriched_codes: std::collections::BTreeSet<String> = Default::default();
    if !codes.is_empty() {
        let req_body = json!({"PackageCodes": codes, "PageNumber": 1, "PageSize": 100});
        for path in [
            "/billing/meter/get-user-resource-paid-packages",
            "/billing/meter/get-user-resource-free-packages",
        ] {
            let Ok((st2, Some(b2), _)) = post_json(&format!("{base}{path}"), headers, &req_body).await
            else {
                continue;
            };
            if st2 == 401 {
                continue;
            }
            if st2 == 200 {
                for p in packages_from(&b2) {
                    if let Some(code) = p.get("code").and_then(Value::as_str) {
                        enriched_codes.insert(code.to_string());
                    }
                    enriched.push(p);
                }
            }
        }
    }

    // summary 自带包：仅补 paid/free 未覆盖的 PackageCode（无到期信息）
    for p in packages_from(&body) {
        let dup = p
            .get("code")
            .and_then(Value::as_str)
            .map(|c| enriched_codes.contains(c))
            .unwrap_or(false);
        if dup {
            continue;
        }
        enriched.push(p);
    }
    (enriched, balance, false, false)
}

/// Buddy 积分看板数据：summary → paid/free 回查 → 旧接口回退 → 备用域名。
pub async fn fetch_credits(st: &super::AdminState) -> Result<Value, String> {
    if st.app.upstream(Some("buddy")).is_none() {
        return Err("Buddy 上游未启用或未找到登录态（CodeBuddyExtension auth）".to_string());
    }
    let headers = billing_headers().await?;

    let mut pkgs: Vec<Value> = vec![];
    let mut balance: Option<f64> = None;
    // 主域名 → 备用域名（仅主域名整体网络不可达时）
    for base in [BILLING_BASE_CN, BILLING_BASE_ALT] {
        let (p, b, saw_auth, net_down) = fetch_round(base, &headers).await;
        pkgs = p;
        balance = b;
        if net_down && pkgs.is_empty() && balance.is_none() {
            continue; // 双探测：切备用域名
        }
        if saw_auth && pkgs.is_empty() && balance.is_none() {
            return Err(format!("登录态失效（{base} 返回 401），请重新登录桌面端"));
        }
        break;
    }

    if pkgs.is_empty() && balance.is_none() {
        // 旧接口回退（F-20）
        let Ok((200, Some(b), _)) = post_json(
            &format!("{BILLING_BASE_CN}/v2/billing/meter/get-user-resource"),
            &headers,
            &old_body(),
        )
        .await
        else {
            return Err("积分接口全部失败（cloud/legacy 均无有效响应）".to_string());
        };
        let old_pkgs = packages_from(&b);
        if !old_pkgs.is_empty() {
            let bal: f64 = old_pkgs
                .iter()
                .filter_map(|p| p.get("remaining").and_then(Value::as_f64))
                .sum();
            return Ok(json!({
                "upstream": "buddy",
                "source": "legacy",
                "remaining_credits": bal,
                "packs": old_pkgs,
            }));
        }
        return Err("积分接口全部失败（旧接口也无有效数据）".to_string());
    }

    // 明细按剩余降序
    pkgs.sort_by(|a, b| {
        b["remaining"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["remaining"].as_f64().unwrap_or(0.0))
    });
    Ok(json!({
        "upstream": "buddy",
        "source": "cloud",
        "remaining_credits": balance,
        "packs": pkgs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_py_truthy_semantics() {
        assert!(!py_truthy(None));
        assert!(!py_truthy(Some(&json!(0))));
        assert!(!py_truthy(Some(&json!(""))));
        assert!(!py_truthy(Some(&Value::Null)));
        assert!(py_truthy(Some(&json!(1))));
        assert!(py_truthy(Some(&json!("x"))));
        assert!(py_truthy(Some(&json!({}))));
    }

    #[test]
    fn test_num_or_none_variants() {
        assert_eq!(num_or_none(Some(&json!(15))), Some(15.0));
        assert_eq!(num_or_none(Some(&json!("12.5"))), Some(12.5));
        assert_eq!(num_or_none(Some(&json!(true))), None);
        assert_eq!(num_or_none(Some(&json!("abc"))), None);
    }

    #[test]
    fn test_packages_from_field_chain() {
        let body = json!({
            "data": {"Packages": [
                {"PackageCode": "P1", "PackageName": "月度包",
                 "CycleCapacitySizePrecise": 1000, "CycleRemainCapacity": 400,
                 "UsedCapacity": 600, "PackageEndTime": "2099-12-31T23:59:59Z"},
                {"PackageCode": "P2", "packageName": "体验包",
                 "CycleRemainCapacity": "50.5", "Remaining": 50.5}
            ]}
        });
        let pkgs = packages_from(&body);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0]["name"], "月度包");
        assert_eq!(pkgs[0]["remaining"], 400.0);
        assert_eq!(pkgs[0]["total"], 1000.0);
        assert_eq!(pkgs[0]["used"], 600.0);
        assert!(pkgs[0]["end_time"].as_str().is_some());
        assert_eq!(pkgs[1]["remaining"], 50.5);
    }

    #[test]
    fn test_balance_from_summary_shapes() {
        // 新结构：data.Packages[] 各包剩余求和
        let body = json!({"data": {"Packages": [
            {"CycleRemainCapacity": 100}, {"CycleRemainCapacity": 250.5}
        ]}});
        assert_eq!(balance_from_summary(&body), Some(350.5));
        // 旧结构：顶层 RemainingCapacity
        let body = json!({"RemainingCapacity": 88});
        assert_eq!(balance_from_summary(&body), Some(88.0));
    }

    #[test]
    fn test_package_codes_dedup() {
        // walk_capable_dicts 只收集带容量键的条目（summary 条目必带）
        let body = json!({"data": {"Packages": [
            {"PackageCode": "A", "CycleRemainCapacity": 100},
            {"PackageCode": "A", "CycleRemainCapacity": 100},
            {"PackageCode": "B", "CycleRemainCapacity": 200}
        ]}});
        assert_eq!(package_codes_from(&body), vec!["A", "B"]);
    }
}
