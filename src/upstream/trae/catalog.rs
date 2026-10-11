//! Trae 模型目录解析（batch_get_detail_param 响应 → ModelInfo）。
//!
//! 解析口径移植自参考实现 TraeWorkAssistant `models_sync.rs`（2026-09-19
//! 三视图逐字段比对 + 客户端两张选择器截图零误差）：
//!
//! - 响应根：`function_configs[]`（每项 {function, config_info_list[]}），
//!   按 `PRIMARY_FUNCTIONS` 优先级跨视图合并、按 config_name 去重；
//! - 客户端可见性核心规则：`context_window_tokens.max ≥ 250k`（当代主力 1M、
//!   Seed 系列 256k；上一代 glm-5.1/kimi-k2.6/max 缺失条目不展示）；
//! - 同名双档：带 `__dev` 的视图是可调用实证，整体替换先前收录的同名条目；
//! - 过滤：内部名（aquila/sagitta 等）、自定义回显（custom_*/usage=custom_model）、
//!   `is_invisible_to_user`、`config_switch=false`、空展示名；
//! - 字段链（实测）：倍率 `display_contact_config`（JSON 字符串）→
//!   `consumption_rate.data.rate`；上下文 `context_window_tokens.{dev,max}`
//!   双口径；档位 `efforts/supported_efforts/thinking_modes`；
//!   图片 `display_config.multimodal`。

use serde_json::Value;

use super::super::ModelInfo;

/// 主语境优先视图（同优先级保持服务端相对顺序）。
const PRIMARY_FUNCTIONS: [&str; 4] = ["solo_agent", "chat_v3", "builder", "builder_v3"];

/// 当代代际阈值：`context_window_tokens.max` 低于此值的不展示
/// （同时天然剔除降级副本与自定义回显条目）。
const CONTEXT_MAX_MIN: u64 = 250_000;

/// 内部/辅助配置名（不可调度）。
fn is_internal(name: &str) -> bool {
    const EXACT: [&str; 4] = ["Doubao_1_6", "doubao_1_6", "aquila", "sagitta"];
    const PREFIXES: [&str; 10] = [
        "custom_model",
        "search_agent",
        "fast_apply",
        "input_optimization",
        "explore",
        "file_search",
        "browser_use",
        "agnes",
        "summary",
        "commit",
    ];
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

fn effort_rank(e: &str) -> usize {
    // Trae 档位命名（light/extra_high）与 Buddy（low/medium/high/max）并轨排序
    ["minimal", "light", "low", "medium", "high", "extra_high", "xhigh", "max"]
        .iter()
        .position(|o| *o == e)
        .unwrap_or(usize::MAX)
}

fn dig_u64(v: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| {
        v.get(*k)
            .and_then(|x| x.as_u64())
            .or_else(|| v.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
    })
}

fn dig_f64(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| {
        v.get(*k)
            .and_then(|x| x.as_f64())
            .or_else(|| v.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
    })
}

fn dig_bool(v: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_bool()))
}

fn dig_strs(v: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|k| {
            v.get(*k)
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .filter(|a| !a.is_empty())
        })
        .unwrap_or_default()
}

/// 单条 config_info → 元数据。
/// 返回 (倍率, 上下文 dev 口径, 上下文 max 口径, 档位, 图片)。
fn extract_meta(ci: &Value) -> (Option<f64>, Option<u64>, Option<u64>, Vec<String>, Option<bool>) {
    // display_contact_config 是 JSON 字符串
    let contact: Value = ci
        .get("display_contact_config")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    let rate = contact
        .get("consumption_rate")
        .and_then(|c| c.get("data"))
        .and_then(|d| dig_f64(d, &["rate"]));
    let ctx = ci.get("context_window_tokens");
    let context_length = ctx.and_then(|c| dig_u64(c, &["dev"]));
    let context_length_max = ctx.and_then(|c| dig_u64(c, &["max"]));
    let mut efforts = dig_strs(
        ci,
        &["efforts", "supported_efforts", "supportedEfforts", "thinking_modes"],
    )
    .into_iter()
    .map(|e| e.to_lowercase())
    .collect::<Vec<_>>();
    efforts.sort_by_key(|e| effort_rank(e));
    efforts.dedup();
    // 实测（2026-10 真机响应）：档位在 reasoning_effort_config.options
    // （如 ["light","high","extra_high"]），并带 default_level / support_thinking。
    // 参考实现挖的顶层 efforts/thinking_modes 键在该形态下不存在。
    if efforts.is_empty() {
        if let Some(rec) = ci.get("reasoning_effort_config") {
            efforts = dig_strs(rec, &["options"]);
        }
    }
    let supports_image = ci
        .get("display_config")
        .and_then(|d| dig_bool(d, &["multimodal"]));
    (rate, context_length, context_length_max, efforts, supports_image)
}

/// 解析 batch_get_detail_param 响应：跨 function 合并、按客户端可见性过滤、去重。
pub fn parse_official_catalog(text: &str) -> Result<Vec<ModelInfo>, String> {
    let root: Value =
        serde_json::from_str(text).map_err(|e| format!("解析失败: {e}"))?;
    let fcs = root
        .get("function_configs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "响应缺少 function_configs".to_string())?;

    // 主语境优先稳定排序（同优先级保持服务端相对顺序）
    let mut ordered_fcs: Vec<&Value> = fcs.iter().collect();
    ordered_fcs.sort_by_key(|fc| {
        let fname = fc.get("function").and_then(|f| f.as_str()).unwrap_or("");
        PRIMARY_FUNCTIONS
            .iter()
            .position(|p| *p == fname)
            .unwrap_or(PRIMARY_FUNCTIONS.len())
    });

    // seen: config_name → 已收录条目是否带 __dev
    let mut seen: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    let mut result: Vec<ModelInfo> = Vec::new();

    for fc in ordered_fcs {
        let Some(items) = fc.get("config_info_list").and_then(|v| v.as_array()) else {
            continue;
        };
        for ci in items {
            let Some(id) = ci
                .get("config_name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if is_internal(&id) {
                continue;
            }
            // 自定义模型回显剔除（客户端主选择器不将其列入标准列表）
            if id.starts_with("custom_")
                || ci.get("usage").and_then(|v| v.as_str()) == Some("custom_model")
                || ci
                    .get("custom_models")
                    .and_then(|v| v.as_array())
                    .map_or(false, |a| !a.is_empty())
            {
                continue;
            }
            if ci
                .get("is_invisible_to_user")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            if ci.get("config_switch").and_then(|v| v.as_bool()) == Some(false) {
                continue;
            }
            let Some(label) = ci
                .get("display_config")
                .and_then(|d| d.get("display_name"))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            // 客户端可见性核心规则：仅展示当代代际（max ≥ 250k）
            let ctx_max = ci
                .get("context_window_tokens")
                .and_then(|c| c.get("max"))
                .and_then(|v| v.as_u64());
            if ctx_max.map_or(true, |m| m < CONTEXT_MAX_MIN) {
                continue;
            }
            let has_dev = ci
                .get("model_detail_list")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().any(|m| {
                        m.get("model_name")
                            .and_then(|v| v.as_str())
                            .map_or(false, |s| s.ends_with("__dev"))
                    })
                })
                .unwrap_or(false);

            let (rate, ctx_dev, ctx_max, efforts, supports_image) = extract_meta(ci);
            let info = ModelInfo {
                id: id.clone(),
                display: label,
                vendor: crate::upstream::codebuddy_catalog::vendor_of(&id).to_string(),
                rate: rate.unwrap_or(0.0),
                efforts,
                // 展示口径：优先 dev（实际请求），缺失回落 max（声明）
                context_length: ctx_dev.or(ctx_max).unwrap_or(0),
                max_tokens: ctx_max.unwrap_or(0),
                supports_image: supports_image.unwrap_or(false),
            };

            match seen.get(&id) {
                Some(true) => {} // 已收录且带 __dev，跳过
                Some(false) => {
                    if has_dev {
                        // 用带 __dev 的条目整体替换先前收录的同名条目
                        if let Some(pos) = result.iter().position(|m| m.id == id) {
                            result[pos] = info;
                            seen.insert(id, true);
                        }
                    }
                }
                None => {
                    result.push(info);
                    seen.insert(id, has_dev);
                }
            }
        }
    }

    if result.is_empty() {
        return Err("目录解析产出 0 个模型（响应结构与预期不符）".to_string());
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// 同步快照（trae_catalog.json；models() 优先消费）
// ---------------------------------------------------------------------------

static CATALOG: std::sync::OnceLock<RwLock<Option<Vec<ModelInfo>>>> = std::sync::OnceLock::new();

use std::sync::RwLock;

fn catalog_cell() -> &'static RwLock<Option<Vec<ModelInfo>>> {
    CATALOG.get_or_init(|| RwLock::new(None))
}

pub fn set_catalog_snapshot(models: Vec<ModelInfo>) {
    *catalog_cell().write().unwrap_or_else(|e| e.into_inner()) = Some(models);
}

pub fn catalog_snapshot() -> Option<Vec<ModelInfo>> {
    catalog_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CatalogFile {
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub fetched_at: String,
}

pub fn save_catalog_snapshot(
    data_dir: &std::path::Path,
    models: &[ModelInfo],
    fetched_at: &str,
) -> Result<(), String> {
    let path = data_dir.join("trae_catalog.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = CatalogFile {
        models: models.to_vec(),
        fetched_at: fetched_at.to_string(),
    };
    let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

pub fn load_catalog_snapshot(data_dir: &std::path::Path) {
    if let Ok(text) = std::fs::read_to_string(data_dir.join("trae_catalog.json")) {
        if let Ok(file) = serde_json::from_str::<CatalogFile>(&text) {
            set_catalog_snapshot(file.models);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_official_basic() {
        let text = serde_json::json!({
            "function_configs": [
                {"function": "chat_v3", "config_info_list": [
                    {"config_name": "DeepSeek-V4-Pro", "display_config": {"display_name": "Deepseek-V4-Pro", "multimodal": true},
                     "context_window_tokens": {"dev": 168000, "max": 1000000},
                     "display_contact_config": "{\"consumption_rate\":{\"enable\":true,\"data\":{\"rate\":\"0.16\"}}}",
                     "reasoning_effort_config": {"default_level": "high", "options": ["light", "high", "extra_high"], "support_thinking": true},
                     "model_detail_list": [{"model_name": "DeepSeek-V4-Pro__dev"}]},
                    {"config_name": "glm-5.1", "display_config": {"display_name": "GLM-5.1"},
                     "context_window_tokens": {"max": 200000}}
                ]},
                {"function": "builder", "config_info_list": [
                    {"config_name": "DeepSeek-V4-Pro", "display_config": {"display_name": "副本"},
                     "context_window_tokens": {"max": 1000000}}
                ]}
            ]
        })
        .to_string();
        let out = parse_official_catalog(&text).unwrap();
        // glm-5.1 max=200k < 250k 被可见性规则剔除；builder 副本被 __dev 优先替换
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "DeepSeek-V4-Pro");
        assert_eq!(out[0].display, "Deepseek-V4-Pro");
        assert_eq!(out[0].rate, 0.16);
        assert_eq!(out[0].context_length, 168000);
        assert_eq!(out[0].max_tokens, 1000000);
        // 实测字段链：reasoning_effort_config.options（light/high/extra_high 命名）
        assert_eq!(out[0].efforts, vec!["light", "high", "extra_high"]);
        assert!(out[0].supports_image);
        assert_eq!(out[0].vendor, "DeepSeek");
    }

    #[test]
    fn test_parse_filters_internal_and_custom() {
        let text = serde_json::json!({
            "function_configs": [
                {"function": "solo_agent", "config_info_list": [
                    {"config_name": "aquila", "display_config": {"display_name": "X"},
                     "context_window_tokens": {"max": 1000000}},
                    {"config_name": "custom_claude", "display_config": {"display_name": "Y"},
                     "context_window_tokens": {"max": 1000000}},
                    {"config_name": "glm-5.3", "usage": "custom_model",
                     "display_config": {"display_name": "Z"}, "context_window_tokens": {"max": 1000000}},
                    {"config_name": "kimi-k3", "display_config": {"display_name": "K"},
                     "context_window_tokens": {"max": 1000000}, "is_invisible_to_user": true},
                    {"config_name": "qwen3.8-max", "display_config": {"display_name": "Q"},
                     "context_window_tokens": {"max": 1000000}, "config_switch": false},
                    {"config_name": "ok-model", "display_config": {"display_name": "OK"},
                     "context_window_tokens": {"max": 1000000}}
                ]}
            ]
        })
        .to_string();
        let out = parse_official_catalog(&text).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "ok-model");
    }

    #[test]
    fn test_parse_empty_rejects() {
        let text = serde_json::json!({"function_configs": []}).to_string();
        assert!(parse_official_catalog(&text).is_err());
    }
}
