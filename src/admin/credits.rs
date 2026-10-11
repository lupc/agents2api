//! Trae 积分看板。
//!
//! 端点（参考 TraeWorkAssistant 逆向实现，本机实测校准）：
//! - 积分用量/余额：`POST https://api.trae.cn/trae/api/v2/pay/ide_user_ent_usage`
//! - 签到积分状态：`POST https://api.trae.cn/trae/api/v2/ug/checkin_credits/status`
//!
//! 响应结构未完全公开，采用防御性解析：收集信封内常见的数值/明细字段，
//! 同时把原始响应一并返回给前端展示（避免静默丢信息）。

use serde_json::{json, Value};

use super::checkin::{find_payload_field, trae_info};
use super::AdminState;

const API_HOST: &str = "https://api.trae.cn";
const ENT_USAGE_URL: &str = "/trae/api/v2/pay/ide_user_ent_usage";
const CHECKIN_STATUS_URL: &str = "/trae/api/v2/ug/checkin_credits/status";

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

/// 拉取 Trae 积分看板数据。
pub async fn fetch(st: &AdminState) -> Result<Value, String> {
    let info = trae_info(st).await?;

    // 两个端点相互独立：任一失败不影响另一个
    let ent_usage = post_billing(&info, ENT_USAGE_URL).await;
    let checkin_status = post_billing(&info, CHECKIN_STATUS_URL).await;

    let mut out = json!({
        "uid": info.user_id,
        "token_expired": info.is_expired(),
    });

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
        // 积分包明细（名称 + 过期时间）
        let packs: Vec<Value> = v
            .pointer("/user_entitlement_pack_list")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|p| {
                        json!({
                            "name": p.get("display_desc").cloned().unwrap_or(Value::Null),
                            "expire_time": p.pointer("/entitlement_base_info/end_time").cloned().unwrap_or(Value::Null),
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
