//! 模型列表：优先读本机 WorkBuddy 的 product.json，失败则用内置兜底列表。

use std::path::PathBuf;

use serde_json::Value;

pub const DEFAULT_MODELS: &[&str] = &[
    "hy3",
    "hy4-preview",
    "kimi-k3",
    "kimi-k2.8-preview",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "glm-5.1",
    "glm-5v-turbo",
    "kimi-k2.7",
    "kimi-k2.6",
    "kimi-k2.5",
    "deepseek-v4-pro",
    "deepseek-v4.1-flash",
    "deepseek-v4-flash",
    "minimax-m3-pay",
    "hy3-preview-agent",
    "auto",
];

/// 标识非聊天模型的 tag（需要过滤掉）
const NON_CHAT_MODEL_TAGS: &[&str] = &["text-to-image", "image-to-image", "text-to-video"];

const NON_CHAT_NAME_KEYWORDS: &[&str] = &["completion", "rewrite", "jump", "codewise"];

fn find_workbuddy_product_json() -> Option<PathBuf> {
    let mut possible: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        possible.push(PathBuf::from(
            "/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/product.json",
        ));
        possible.push(dirs_home().join(
            "Desktop/workspace/opensource/codebuddy2api/workbuddy_extracted/cli/product.json",
        ));
    } else if cfg!(target_os = "windows") {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            possible.push(PathBuf::from(local)
                .join("Programs")
                .join("WorkBuddy")
                .join("resources")
                .join("app.asar.unpacked")
                .join("cli")
                .join("product.json"));
        }
        possible.push(PathBuf::from(
            "C:/Program Files/WorkBuddy/resources/app.asar.unpacked/cli/product.json",
        ));
    } else {
        possible.push(PathBuf::from(
            "/opt/WorkBuddy/resources/app.asar.unpacked/cli/product.json",
        ));
        possible.push(dirs_home().join(".local/share/WorkBuddy/cli/product.json"));
    }
    possible.into_iter().find(|p| p.is_file())
}

/// 从本机 WorkBuddy product.json 读取模型列表。
///
/// 过滤规则：
/// 1. 只保留聊天模型（排除 text-to-image, text-to-video 等）
/// 2. 排除 vendor 为 "tencent" 的内部模型（通常是补全/内部专用）
/// 3. 返回模型 ID 列表
fn load_models_from_workbuddy() -> Vec<String> {
    let Some(path) = find_workbuddy_product_json() else {
        return Vec::new();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            crate::config::log_at(
                crate::config::Level::Warn,
                &format!("Failed to load models from WorkBuddy product.json: {e}"),
            );
            return Vec::new();
        }
    };
    let data: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            crate::config::log_at(
                crate::config::Level::Warn,
                &format!("Failed to parse WorkBuddy product.json: {e}"),
            );
            return Vec::new();
        }
    };
    let mut chat_models = Vec::new();
    for model in data.get("models").and_then(|m| m.as_array()).unwrap_or(&vec![]) {
        let Some(model_id) = model.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        // 过滤掉非聊天模型
        let tags: Vec<&str> = model
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
            .unwrap_or_default();
        if tags.iter().any(|t| NON_CHAT_MODEL_TAGS.contains(t)) {
            continue;
        }
        // 过滤掉内部模型（vendor 为 tencent 的通常是补全/跳转等内部功能）
        let vendor = model.get("vendor").and_then(|v| v.as_str()).unwrap_or("");
        if vendor == "tencent" {
            continue;
        }
        // 过滤掉名称中明显是补全/内部功能的模型
        let name_lower = model_id.to_lowercase();
        if NON_CHAT_NAME_KEYWORDS.iter().any(|k| name_lower.contains(k)) {
            continue;
        }
        chat_models.push(model_id.to_string());
    }
    chat_models
}

/// 获取可用的模型列表。优先 WorkBuddy 动态目录，失败则内置兜底。
pub fn get_available_models() -> Vec<String> {
    let workbuddy_models = load_models_from_workbuddy();
    if !workbuddy_models.is_empty() {
        workbuddy_models
    } else {
        DEFAULT_MODELS.iter().map(|s| s.to_string()).collect()
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
