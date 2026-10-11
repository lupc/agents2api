//! 读取本机 OpenCode 的 `auth.json`，取出其中标准 OpenAI 兼容的服务商凭据。
//!
//! OpenCode 把 `/connect` 添加的 key 存在 `auth.json`（各平台路径见下），结构形如：
//! ```json
//! { "nvidia": {"type": "api", "key": "nvapi-..."},
//!   "modelscope": {"type": "api", "key": "ms-..."} }
//! ```
//! 这里只挑出已知是「标准 OpenAI 兼容」的服务商（`nvidia` / `modelscope`），
//! 并解析出各自的 base URL。

use std::path::PathBuf;

use serde_json::Value;

/// 已知的、标准 OpenAI 兼容的 OpenCode 服务商：(provider 名, base URL)。
///
/// base URL 均为不带 `/chat/completions` 的前缀，由 [`super::OpenCodeKeyUpstream`] 拼接。
pub const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    ("nvidia", "https://integrate.api.nvidia.com/v1"),
    ("modelscope", "https://api-inference.modelscope.cn/v1"),
];

/// auth.json 的候选路径（可用 `OPENCODE_AUTH_JSON` 覆盖）。
pub fn auth_json_candidates() -> Vec<PathBuf> {
    if let Ok(explicit) = std::env::var("OPENCODE_AUTH_JSON") {
        if !explicit.is_empty() {
            return vec![PathBuf::from(explicit)];
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    // OpenCode 的数据目录：Windows 用 %APPDATA%，其他平台用 XDG / ~/.local/share
    if cfg!(target_os = "windows") {
        if let Ok(appdata) = std::env::var("APPDATA") {
            out.push(PathBuf::from(appdata).join("opencode").join("auth.json"));
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            out.push(
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("opencode")
                    .join("auth.json"),
            );
        }
    } else {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            out.push(PathBuf::from(xdg).join("opencode").join("auth.json"));
        }
        if let Ok(home) = std::env::var("HOME") {
            out.push(
                PathBuf::from(&home)
                    .join(".local")
                    .join("share")
                    .join("opencode")
                    .join("auth.json"),
            );
        }
    }
    // macOS 也可能落在 ~/.config
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        out.push(
            PathBuf::from(home)
                .join(".config")
                .join("opencode")
                .join("auth.json"),
        );
    }
    out
}

pub fn find_auth_json() -> Option<PathBuf> {
    auth_json_candidates().into_iter().find(|p| p.is_file())
}

/// 从解析后的 auth.json 里挑出一个已知兼容服务商的 (name, base_url, key)。
pub fn extract_provider(root: &Value) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (name, base) in KNOWN_PROVIDERS {
        let Some(entry) = root.get(*name) else {
            continue;
        };
        if let Some(key) = entry.get("key").and_then(|k| k.as_str()) {
            if !key.is_empty() {
                out.push((name.to_string(), base.to_string(), key.to_string()));
            }
        }
    }
    out
}

/// 读取本机 auth.json 并返回所有已知兼容服务商的凭据。
pub fn load_providers() -> Vec<(String, String, String)> {
    let Some(path) = find_auth_json() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    extract_provider(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_known_providers() {
        let root = json!({
            "nvidia": {"type": "api", "key": "nvapi-x"},
            "modelscope": {"type": "api", "key": "ms-y"},
            "someother": {"type": "api", "key": "z"},
        });
        let got = extract_provider(&root);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "nvidia");
        assert_eq!(got[0].1, "https://integrate.api.nvidia.com/v1");
        assert_eq!(got[0].2, "nvapi-x");
        assert_eq!(got[1].0, "modelscope");
    }

    #[test]
    fn test_extract_skips_empty_key() {
        let root = json!({ "nvidia": {"type": "api", "key": ""} });
        assert!(extract_provider(&root).is_empty());
    }

    #[test]
    fn test_extract_ignores_unknown() {
        let root = json!({ "openai": {"type": "api", "key": "sk-x"} });
        assert!(extract_provider(&root).is_empty());
    }
}
