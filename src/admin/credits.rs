//! Trae 积分看板。
//!
//! 端点（参考 TraeWorkAssistant 逆向实现，本机实测校准）：
//! - 积分用量/余额：`POST https://api.trae.cn/trae/api/v2/pay/ide_user_ent_usage`
//! - 签到积分状态：`POST https://api.trae.cn/trae/api/v2/ug/checkin_credits/status`
//! - 今日会话消耗：`POST https://api.trae.cn/trae/api/v1/pay/query_user_usage_group_by_session`
//!   （v1 接口，需 Web 控制台形态请求头，usage_type=[7]）
//!
//! 响应结构未完全公开，采用防御性解析：收集信封内常见的数值/明细字段，
//! 同时把原始响应一并返回给前端展示（避免静默丢信息）。

use serde_json::{json, Value};

use super::checkin::{find_payload_field, trae_info};
use super::AdminState;

const API_HOST: &str = "https://api.trae.cn";
const ENT_USAGE_URL: &str = "/trae/api/v2/pay/ide_user_ent_usage";
const CHECKIN_STATUS_URL: &str = "/trae/api/v2/ug/checkin_credits/status";
const USAGE_SESSION_URL: &str = "/trae/api/v1/pay/query_user_usage_group_by_session";

fn billing_headers(
    info: &crate::upstream::trae::auth::TraeAuthInfo,
) -> Vec<(String, String)> {
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

/// Web 控制台形态请求头（v1 接口要求，不能带 IDE 指纹头）。
fn web_headers(info: &crate::upstream::trae::auth::TraeAuthInfo) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "authorization".to_string(),
            format!("Cloud-IDE-JWT {}", info.token),
        ),
        (
            "user-agent".to_string(),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36".to_string(),
        ),
        ("origin".to_string(), "https://www.trae.cn".to_string()),
        ("referer".to_string(), "https://www.trae.cn/".to_string()),
        ("sec-fetch-dest".to_string(), "empty".to_string()),
        ("sec-fetch-mode".to_string(), "cors".to_string()),
        ("sec-fetch-site".to_string(), "same-site".to_string()),
    ]
}

async fn post_billing(
    info: &crate::upstream::trae::auth::TraeAuthInfo,
    path: &str,
) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{API_HOST}{path}"));
    for (k, v) in billing_headers(info) {
        req = req.header(k, v);
    }
    let resp = req
        .json(&json!({}))
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("网络错误: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("读取失败: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| {
        format!(
            "响应解析失败: {e}；原始: {:?}",
            text.chars().take(200).collect::<String>()
        )
    })
}

/// 拉取今日会话级消耗积分（v1 接口，Web 控制台形态）。
///
/// 请求 `query_user_usage_group_by_session`，按 `usage_type=[7]`（Cloud-IDE
/// 会话积分消耗）过滤，对返回的 `user_usage_group_by_sessions[].credits_float`
/// 求和。参考 TraeWorkAssistant `usage_history.rs`。
async fn fetch_today_usage(
    info: &crate::upstream::trae::auth::TraeAuthInfo,
) -> Result<f64, String> {
    let now = chrono::Local::now();
    let today_midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(chrono::Local)
        .single()
        .ok_or("时区转换失败")?;
    let start_ts = today_midnight.timestamp();
    let end_ts = now.timestamp();

    let client = reqwest::Client::new();
    let mut total_credits = 0.0_f64;
    let mut page_num = 1_i64;
    let page_size = 20_i64;
    let mut total: i64 = 0;
    let mut got = 0_i64;

    loop {
        let body = json!({
            "start_time": start_ts,
            "end_time": end_ts,
            "page_size": page_size,
            "page_num": page_num,
            "usage_type": [7],
        });
        let mut req = client.post(format!("{API_HOST}{USAGE_SESSION_URL}"));
        for (k, v) in web_headers(info) {
            req = req.header(k, v);
        }
        let resp = req
            .json(&body)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("今日消耗网络错误: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("读取失败: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "今日消耗 HTTP {status}: {}",
                text.chars().take(200).collect::<String>()
            ));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| {
            format!(
                "今日消耗响应解析失败: {e}；原始: {:?}",
                text.chars().take(200).collect::<String>()
            )
        })?;
        if total == 0 {
            total = v.get("total").and_then(|t| t.as_i64()).unwrap_or(0);
        }
        let sessions = v
            .get("user_usage_group_by_sessions")
            .and_then(|s| s.as_array());
        match sessions {
            Some(arr) if !arr.is_empty() => {
                got += arr.len() as i64;
                for s in arr {
                    let credits = s
                        .get("credits_float")
                        .and_then(|c| c.as_f64())
                        .or_else(|| s.get("amount_float").and_then(|c| c.as_f64()))
                        .unwrap_or(0.0);
                    total_credits += credits;
                }
                if got >= total || arr.len() < page_size as usize {
                    break;
                }
            }
            _ => break,
        }
        page_num += 1;
    }

    Ok(total_credits)
}

/// 拉取 Trae 积分看板数据。
pub async fn fetch(st: &AdminState) -> Result<Value, String> {
    let info = trae_info(st).await?;

    // 三个端点相互独立：任一失败不影响另一个
    let ent_usage = post_billing(&info, ENT_USAGE_URL).await;
    let checkin_status = post_billing(&info, CHECKIN_STATUS_URL).await;
    let today_usage = fetch_today_usage(&info).await;

    let mut out = json!({
        "uid": info.user_id,
        "token_expired": info.is_expired(),
    });

    // 今日消耗积分（v1 会话级接口）
    match &today_usage {
        Ok(credits) => out["today_consumed"] = json!(credits),
        Err(e) => out["today_consumed_error"] = json!(e),
    }

    if let Ok(v) = &ent_usage {
        // 实测响应形状（2026-10）：
        // usage_summary: { consumed_amount, consumption_ratio, total_amount }
        // user_entitlement_pack_list[]: { display_desc, entitlement_base_info: { end_time, ... } }
        let usage = v.pointer("/usage_summary");
        let total = usage
            .and_then(|u| u.get("total_amount"))
            .and_then(|t| t.as_f64())
            .or_else(|| find_payload_field(v, "total_amount", 0).and_then(|t| t.as_f64()))
            .unwrap_or(0.0);
        let consumed = usage
            .and_then(|u| u.get("consumed_amount"))
            .and_then(|t| t.as_f64())
            .unwrap_or(0.0);
        out["total_credits"] = json!(total);
        out["consumed_credits"] = json!(consumed);
        out["remaining_credits"] = json!((total - consumed).max(0.0));
        out["consumption_ratio"] = usage
            .and_then(|u| u.get("consumption_ratio"))
            .cloned()
            .unwrap_or(Value::Null);
        // 积分明细：获得 = quota.credits_limit；已用 = usage.credits_amount；
        // 剩余 = 获得 - 已用（实测字段链：entitlement_base_info.quota.credits_limit）
        let packs: Vec<Value> = v
            .pointer("/user_entitlement_pack_list")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|p| {
                        let granted = p
                            .pointer("/entitlement_base_info/quota/credits_limit")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        let used = p
                            .pointer("/usage/credits_amount")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        // 过期时间：unix 秒 → 本地日期串（前端读 end_time）
                        let end_ts = p
                            .pointer("/entitlement_base_info/end_time")
                            .and_then(|v| v.as_i64());
                        let end_time = end_ts
                            .and_then(crate::admin::buddy_billing::fmt_ts_local);
                        json!({
                            "name": p.get("display_desc").cloned().unwrap_or(Value::Null),
                            "granted": granted,
                            "remaining": (granted - used).max(0.0),
                            "used": used,
                            "end_time": end_time.unwrap_or_else(|| "—".to_string()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out["packs"] = json!(packs);
        out["ent_usage_raw"] = v.clone();
    } else if let Err(e) = &ent_usage {
        out["ent_usage_error"] = json!(e);
    }

    if let Ok(v) = &checkin_status {
        out["checked_in"] = find_payload_field(v, "checked_in", 0).unwrap_or(Value::Null);
        out["checkin_status_raw"] = v.clone();
    } else if let Err(e) = &checkin_status {
        out["checkin_status_error"] = json!(e);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_billing_headers_format() {
        let info = crate::upstream::trae::auth::TraeAuthInfo {
            token: "jwt-t".to_string(),
            user_id: "u9".to_string(),
            device_id: "d9".to_string(),
            machine_id: "m9".to_string(),
            expires_at_ms: 0,
            host_variant: "Trae CN".to_string(),
            storage_path: std::path::PathBuf::from("/fake"),
        };
        let map: std::collections::HashMap<_, _> = billing_headers(&info).into_iter().collect();
        assert_eq!(map.get("authorization").unwrap(), "Cloud-IDE-JWT jwt-t");
        assert_eq!(map.get("x-market-user-id").unwrap(), "u9");
    }
}
