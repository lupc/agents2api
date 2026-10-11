//! CodeBuddy（copilot.tencent.com）远程模型目录。
//!
//! 端点与解析逻辑移植自参考实现 TraeWorkAssistant 的 `wb_catalog.rs`
//! （2026-09 抓包实证）：
//! - `GET {backend}/console/enterprises/personal/models`（与 chat 同域同鉴权族）
//! - 响应形态多变：`data.models[]` / `data.agents[]`（agent 内嵌 models 子数组）/
//!   根数组等，需定向容器探测 + 深扫兜底
//! - 官方客户端菜单口径 = `cli` agent（tags 含 cli+default）白名单 ∩ data.models，
//!   再剔路由别名黑名单（auto/default/hunyuan-chat）
//! - 能力字段链（实测）：倍率 `credits: "x0.05"`、档位 `reasoning.supportedEfforts`、
//!   上下文 `maxInputTokens`、图片 `supportsImages`（`disabledMultimodal` 显式禁用）
//! - 质量门：解析产出全零能力字段 = 字段链漂移，拒绝落盘保持旧目录

use std::collections::BTreeSet;
use std::sync::RwLock;

use serde_json::Value;

use super::ModelInfo;

/// 模型条目能力特征键（命中其一才算模型条目，剔 agent/命令噪音）。
const LOOKS_LIKE_KEYS: &[&str] = &[
    "maxOutputTokens",
    "maxInputTokens",
    "maxTokens",
    "max_tokens",
    "credits",
    "rate",
    "ratio",
    "price",
    "creditRate",
    "contextLength",
    "context_length",
    "inputTokenLimit",
    "inputModalities",
    "input_modalities",
    "modalities",
    "supportsImages",
    "reasoning",
    "supportedEfforts",
    "supported_efforts",
];

/// 噪音模型 id 黑名单：路由别名 / 内部条目，不是可调度的真实模型
/// （参考实现 2026-09-19 用户确认 + 抓包实证）。
fn is_noise_model_id(id: &str) -> bool {
    matches!(id, "auto" | "default" | "hunyuan-chat")
}

/// 官方客户端过滤逻辑逆向：客户端模型菜单 = `cli` agent（tags 含 cli+default）
/// 的 models 白名单 ∩ data.models。返回 None = 响应无 cli 主 agent（旧形态），
/// 此时不做白名单过滤。
fn cli_agent_whitelist(body: &Value) -> Option<Vec<String>> {
    let agents = body
        .get("data")
        .and_then(|d| d.get("agents"))
        .or_else(|| body.get("agents"))?
        .as_array()?;
    for a in agents {
        let tags: Vec<&str> = a
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();
        if !tags.contains(&"cli") || !tags.contains(&"default") {
            continue;
        }
        let ids: Vec<String> = a
            .get("models")?
            .as_array()?
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
        if !ids.is_empty() {
            return Some(ids);
        }
    }
    None
}

/// 从单条候选对象解析模型（字段链宽容探测，能力字段读上游勿硬编码）。
fn model_from_item(item: &Value) -> Option<ModelInfo> {
    if !item.is_object() {
        return None;
    }
    let dig_str = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            item.get(*k)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .or_else(|| item.get(*k).and_then(|x| x.as_i64()).map(|n| n.to_string()))
        })
    };
    let dig_u64 = |keys: &[&str]| -> Option<u64> {
        keys.iter().find_map(|k| {
            item.get(*k)
                .and_then(|x| x.as_u64())
                .or_else(|| item.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
        })
    };
    let dig_f64 = |keys: &[&str]| -> Option<f64> {
        keys.iter().find_map(|k| {
            item.get(*k)
                .and_then(|x| x.as_f64())
                .or_else(|| item.get(*k).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
        })
    };
    let dig_strs = |v: &Value, keys: &[&str]| -> Vec<String> {
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
    };

    let id = dig_str(&["id", "model", "modelId", "name"])?
        .trim()
        .to_lowercase();
    if id.is_empty() {
        return None;
    }

    // 倍率：credits 字符串形如 "x0.05" / "x0.00 credits"；数字或旧键兜底
    let rate = dig_f64(&["rate", "ratio", "price", "creditRate"]).unwrap_or_else(|| {
        item.get("credits")
            .and_then(|x| match x {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s
                    .trim()
                    .trim_start_matches(['x', 'X'])
                    .trim()
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok()),
                _ => None,
            })
            .unwrap_or(0.0)
    });

    // 档位：顶层旧键 → reasoning.supportedEfforts 数组 → reasoning.effort 单档
    let reasoning = item.get("reasoning");
    let mut efforts = dig_strs(item, &["supportedEfforts", "supported_efforts", "efforts"]);
    if efforts.is_empty() {
        if let Some(r) = reasoning {
            efforts = dig_strs(r, &["supportedEfforts", "supported_efforts", "efforts"]);
            if efforts.is_empty() {
                if let Some(e) = r.get("effort").and_then(|x| x.as_str()) {
                    if !e.trim().is_empty() {
                        efforts.push(e.trim().to_string());
                    }
                }
            }
        }
    }
    let efforts: Vec<String> = efforts.into_iter().map(|e| e.to_lowercase()).collect();

    // 图片：supportsImages 布尔（disabledMultimodal 显式禁用）；旧模态数组兜底
    let disabled_multimodal = item
        .get("disabledMultimodal")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let modalities = dig_strs(item, &["inputModalities", "input_modalities", "modalities"]);
    let supports_image = match item.get("supportsImages").and_then(Value::as_bool) {
        Some(b) => b && !disabled_multimodal,
        None => modalities
            .iter()
            .any(|m| m.eq_ignore_ascii_case("image") || m.eq_ignore_ascii_case("image_url")),
    };

    Some(ModelInfo {
        display: dig_str(&["displayName", "display", "name"]).unwrap_or_else(|| id.clone()),
        context_length: dig_u64(&[
            "contextLength",
            "context_length",
            "inputTokenLimit",
            "maxInputTokens",
            "maxAllowedSize",
        ])
        .unwrap_or_else(|| {
            item.get("contextWindow")
                .and_then(|c| c.get("defaultLength"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        }),
        max_tokens: dig_u64(&["maxTokens", "max_tokens", "maxOutputTokens"]).unwrap_or(0),
        supports_image,
        efforts,
        rate,
        id,
        vendor: String::new(), // 由 vendor_of 按前缀推断
    })
}

/// 定向容器收集：从响应中找出「模型条目」候选对象列表。
/// 兼容形态：根数组 / {data|models|list|items|agents|entries|result}；
/// agent 条目内嵌 models/list/items 子数组时展开子条目。
fn collect_candidates(v: &Value, out: &mut Vec<Value>, depth: usize) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Array(arr) => {
            for el in arr {
                let mut nested = false;
                for key in ["models", "list", "items"] {
                    if let Some(sub) = el.get(key).and_then(Value::as_array) {
                        collect_candidates(&Value::Array(sub.clone()), out, depth + 1);
                        nested = true;
                    }
                }
                if !nested {
                    out.push(el.clone());
                }
            }
        }
        Value::Object(obj) => {
            for key in ["data", "models", "list", "items", "agents", "entries", "result"] {
                if let Some(sub) = obj.get(key) {
                    if sub.is_array() || sub.is_object() {
                        collect_candidates(sub, out, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

/// 深度扫描兜底（定向解析产出 0 条时）：全树递归收集有显式 id 键的对象。
fn deep_scan_models(v: &Value, out: &mut Vec<Value>, depth: usize) {
    if depth > 8 {
        return;
    }
    match v {
        Value::Array(arr) => {
            for x in arr {
                deep_scan_models(x, out, depth + 1);
            }
        }
        Value::Object(_) => {
            let id_like = ["id", "model", "modelId", "model_id"].iter().any(|k| {
                matches!(v.get(*k), Some(Value::String(s)) if !s.trim().is_empty())
            });
            if id_like {
                out.push(v.clone());
            }
            if let Some(obj) = v.as_object() {
                for val in obj.values() {
                    deep_scan_models(val, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// 上游目录响应 → 模型列表（宽容解析 + 白名单/黑名单过滤 + 按 id 去重）。
/// 产出 0 条或能力字段全零（字段链漂移信号）视为失败。
pub fn parse_upstream_catalog(body: &Value) -> Result<Vec<ModelInfo>, String> {
    let mut candidates: Vec<Value> = Vec::new();
    collect_candidates(body, &mut candidates, 0);
    let whitelist = cli_agent_whitelist(body);
    let admitted = |m: &ModelInfo| -> bool {
        !is_noise_model_id(&m.id)
            && whitelist
                .as_ref()
                .map_or(true, |w| w.iter().any(|x| x.eq_ignore_ascii_case(&m.id)))
    };

    let mut out: Vec<ModelInfo> = Vec::new();
    let mut seen = BTreeSet::new();
    for item in &candidates {
        if !looks_like_model(item) {
            continue;
        }
        if let Some(mut m) = model_from_item(item) {
            m.vendor = vendor_of(&m.id).to_string();
            if admitted(&m) && seen.insert(m.id.clone()) {
                out.push(m);
            }
        }
    }
    if out.is_empty() {
        // 深扫兜底
        let mut scanned: Vec<Value> = Vec::new();
        deep_scan_models(body, &mut scanned, 0);
        for item in &scanned {
            if !looks_like_model(item) {
                continue;
            }
            if let Some(mut m) = model_from_item(item) {
                m.vendor = vendor_of(&m.id).to_string();
                if admitted(&m) && seen.insert(m.id.clone()) {
                    out.push(m);
                }
            }
        }
    }
    if out.is_empty() {
        let excerpt: String = serde_json::to_string(body)
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(format!(
            "上游目录解析产出 0 个模型（响应结构与预期不符，body 摘要: {excerpt}）"
        ));
    }
    // 质量门：全部条目能力字段全零 = 字段链漂移（参考实现 2026-09 实测教训）
    if out
        .iter()
        .all(|m| m.rate == 0.0 && m.efforts.is_empty() && m.context_length == 0)
    {
        return Err(format!(
            "上游目录 {} 个条目能力字段全空（倍率/档位/上下文均未解析到，字段链疑似漂移），拒绝落盘",
            out.len()
        ));
    }
    Ok(out)
}

fn looks_like_model(item: &Value) -> bool {
    LOOKS_LIKE_KEYS.iter().any(|k| item.get(*k).is_some())
}

/// 按模型 id 前缀推断厂商（对齐参考实现 unified_catalog::vendor_of）。
pub fn vendor_of(canonical: &str) -> &'static str {
    let lower = canonical.to_lowercase();
    for (prefix, vendor) in [
        ("glm-", "智谱"),
        ("deepseek", "DeepSeek"),
        ("kimi", "Moonshot"),
        ("doubao", "字节·豆包"),
        ("hy3", "腾讯·混元"),
        ("hy4", "腾讯·混元"),
        ("qwen", "阿里·通义"),
        ("minimax", "MiniMax"),
        ("space-bunny", "MiniMax"),
        ("claude", "Anthropic"),
        ("gemini", "Google"),
        ("grok", "xAI"),
        ("gpt", "OpenAI"),
        ("o1", "OpenAI"),
        ("o3", "OpenAI"),
        ("step-5", "阶跃星辰"),
        ("step5", "阶跃星辰"),
        ("mimo", "小米"),
        ("hunyuan", "腾讯·混元"),
    ] {
        if lower.starts_with(prefix) {
            return vendor;
        }
    }
    "其他"
}

// ---------------------------------------------------------------------------
// 同步快照（全局）：models() 优先消费已同步目录，管理后台写入
// ---------------------------------------------------------------------------

static CATALOG: std::sync::OnceLock<RwLock<Option<Vec<ModelInfo>>>> = std::sync::OnceLock::new();

fn catalog_cell() -> &'static RwLock<Option<Vec<ModelInfo>>> {
    CATALOG.get_or_init(|| RwLock::new(None))
}

/// 写入目录快照（同步成功后调用；重启时由 main 从 data_dir 载入）。
pub fn set_catalog_snapshot(models: Vec<ModelInfo>) {
    *catalog_cell().write().unwrap_or_else(|e| e.into_inner()) = Some(models);
}

/// 读取目录快照。
pub fn catalog_snapshot() -> Option<Vec<ModelInfo>> {
    catalog_cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 从磁盘载入快照（启动时）。
pub fn load_catalog_snapshot(data_dir: &std::path::Path) {
    let path = data_dir.join("wb_model_catalog.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(file) = serde_json::from_str::<CatalogFile>(&text) {
            set_catalog_snapshot(file.models);
        }
    }
}

/// 目录落盘结构。
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CatalogFile {
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub fetched_at: String,
}

/// 保存目录快照到磁盘（原子写）。
pub fn save_catalog_snapshot(data_dir: &std::path::Path, models: &[ModelInfo], fetched_at: &str) -> Result<(), String> {
    let path = data_dir.join("wb_model_catalog.json");
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_data_models_with_credits_string() {
        let body = json!({
            "code": 0,
            "data": {
                "models": [
                    {"id": "GLM-5.3", "displayName": "GLM-5.3", "credits": "x0.79 credits",
                     "maxInputTokens": 1000000, "maxOutputTokens": 96000,
                     "reasoning": {"supportedEfforts": ["low", "high", "max"]}, "supportsImages": true},
                    {"id": "hy3", "credits": "x0.00", "maxInputTokens": 192000,
                     "reasoning": {"effort": "high"}, "supportsImages": true}
                ]
            }
        });
        let out = parse_upstream_catalog(&body).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "glm-5.3");
        assert_eq!(out[0].rate, 0.79);
        assert_eq!(out[0].context_length, 1000000);
        assert_eq!(out[0].efforts, vec!["low", "high", "max"]);
        assert!(out[0].supports_image);
        assert_eq!(out[0].vendor, "智谱");
        assert_eq!(out[1].id, "hy3");
        assert_eq!(out[1].efforts, vec!["high"]);
        assert_eq!(out[1].vendor, "腾讯·混元");
    }

    #[test]
    fn test_whitelist_and_noise_filtering() {
        let body = json!({
            "data": {
                "agents": [
                    {"tags": ["cli", "default"], "models": ["glm-5.2", "auto", "default"]},
                    {"tags": ["other"], "models": ["secret-model"]}
                ],
                "models": [
                    {"id": "glm-5.2", "credits": "x0.79", "maxInputTokens": 1},
                    {"id": "auto", "credits": "x1.00", "maxInputTokens": 1},
                    {"id": "default", "credits": "x2.00", "maxInputTokens": 1},
                    {"id": "hunyuan-chat", "maxInputTokens": 1},
                    {"id": "secret-model", "credits": "x1.00", "maxInputTokens": 1}
                ]
            }
        });
        let out = parse_upstream_catalog(&body).unwrap();
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        // auto/default 黑名单剔除；secret-model 不在 cli 白名单内
        assert_eq!(ids, vec!["glm-5.2"]);
    }

    #[test]
    fn test_quality_gate_rejects_all_zero() {
        // 有 id 无任何能力字段 = 字段链漂移，应拒绝
        let body = json!({"data": {"models": [
            {"id": "m1"}, {"id": "m2"}
        ]}});
        assert!(parse_upstream_catalog(&body).is_err());
    }

    #[test]
    fn test_disabled_multimodal_and_modality_fallback() {
        let body = json!({"data": {"models": [
            {"id": "a", "credits": "x0.1", "supportsImages": true, "disabledMultimodal": true},
            {"id": "b", "credits": "x0.1", "inputModalities": ["text", "image"]}
        ]}});
        let out = parse_upstream_catalog(&body).unwrap();
        assert!(!out[0].supports_image);
        assert!(out[1].supports_image);
    }

    #[test]
    fn test_vendor_of_prefixes() {
        assert_eq!(vendor_of("glm-5.3"), "智谱");
        assert_eq!(vendor_of("deepseek-v4-pro"), "DeepSeek");
        assert_eq!(vendor_of("kimi-k3-1"), "Moonshot");
        assert_eq!(vendor_of("hy3-preview"), "腾讯·混元");
        assert_eq!(vendor_of("minimax-m3"), "MiniMax");
        assert_eq!(vendor_of("space-bunny"), "MiniMax");
        assert_eq!(vendor_of("unknown-x"), "其他");
    }
}
