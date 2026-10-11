//! 上游可改配置：文件持久化 + 注册表重建。
//!
//! `<data_dir>/upstreams.json` 结构：
//! ```json
//! {
//!   "default_upstream": "codebuddy",
//!   "upstreams": {
//!     "codebuddy": { "enabled": true, "backend": "https://copilot.tencent.com" },
//!     "workbuddy": { "enabled": true },
//!     "trae":      { "enabled": true, "chat_host": "https://trae-api-cn.mchost.guru" },
//!     "opencode":  { "enabled": true }
//!   }
//! }
//! ```
//! 语义：文件值**覆盖** CLI/env 的同名设置（文件缺省字段 = 用 CLI/env 值）；
//! `enabled: false` 移除该上游；`opencode` 键控制全部 opencode providers。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::upstream::{Upstream, UpstreamRegistry};

/// 重建注册表所需的启动期基础设置（来自 CLI/env，reload 时作为兜底）。
#[derive(Clone, Default)]
pub struct BaseSettings {
    pub codebuddy_backend: Option<String>,
    pub trae_chat_host: Option<String>,
    pub trae_storage_json: Option<String>,
    pub opencode_auth_json: Option<String>,
}

/// 单上游的文件覆盖项。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpstreamOverride {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// codebuddy 专用：后端地址覆盖。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// trae 专用：chat host 覆盖。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_host: Option<String>,
    /// trae 专用：storage.json 路径覆盖。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_json: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 整个配置文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamsConfig {
    #[serde(default)]
    pub default_upstream: Option<String>,
    #[serde(default)]
    pub upstreams: HashMap<String, UpstreamOverride>,
}

impl UpstreamsConfig {
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("upstreams.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Self {
                default_upstream: None,
                upstreams: HashMap::new(),
            })
    }

    pub fn save(data_dir: &Path, cfg: &UpstreamsConfig) -> Result<(), String> {
        let path = data_dir.join("upstreams.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }

    pub fn ov(&self, name: &str) -> Option<&UpstreamOverride> {
        self.upstreams.get(name)
    }

    pub fn enabled(&self, name: &str, cli_disabled: bool) -> bool {
        if cli_disabled {
            return false;
        }
        self.ov(name).map(|o| o.enabled).unwrap_or(true)
    }
}

/// 构建（或重建）上游注册表：CLI 基础设置 + 文件覆盖项合并。
///
/// 启动与 reload 走同一路径；凭据路径的自动探测逻辑与首建一致。
pub fn build_registry(
    base: &BaseSettings,
    cli_disabled: &[String],
    overrides: &UpstreamsConfig,
) -> UpstreamRegistry {
    use crate::auth::CredentialManager;
    use crate::upstream::codebuddy::CodeBuddyUpstream;
    use crate::upstream::trae::{auth::TraeCredential, TraeUpstream};

    let mut map: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
    let disabled = |name: &str| !overrides.enabled(name, cli_disabled.iter().any(|d| d == name));

    // CodeBuddy / WorkBuddy 同一上游、两个别名
    if !disabled("codebuddy") && !disabled("workbuddy") {
        if let Some(path) = crate::auth::find_auth_file() {
            let backend = overrides
                .ov("codebuddy")
                .and_then(|o| o.backend.clone())
                .or_else(|| base.codebuddy_backend.clone());
            let inst: Arc<dyn Upstream> =
                Arc::new(CodeBuddyUpstream::new(CredentialManager::new(path), backend));
            map.insert("codebuddy".to_string(), inst.clone());
            map.insert("workbuddy".to_string(), inst);
        }
    }

    // Trae
    if !disabled("trae") {
        let storage = overrides
            .ov("trae")
            .and_then(|o| o.storage_json.clone())
            .or_else(|| base.trae_storage_json.clone())
            .map(PathBuf::from)
            .or_else(crate::upstream::trae::auth::find_storage_json);
        if let Some(path) = storage {
            let chat_host = overrides
                .ov("trae")
                .and_then(|o| o.chat_host.clone())
                .or_else(|| base.trae_chat_host.clone());
            map.insert(
                "trae".to_string(),
                Arc::new(TraeUpstream::new(TraeCredential::new(path), chat_host)),
            );
        }
    }

    // OpenCode providers（"opencode" 键统一开关）
    if !disabled("opencode") {
        if let Some(explicit) = &base.opencode_auth_json {
            std::env::set_var("OPENCODE_AUTH_JSON", explicit);
        }
        for (name, base_url, key) in crate::upstream::opencode::auth::load_providers() {
            if !disabled(&name) {
                map.insert(
                    name.clone(),
                    Arc::new(crate::upstream::opencode::OpenCodeKeyUpstream::new(
                        name, base_url, key,
                    )),
                );
            }
        }
    }

    let default = overrides
        .default_upstream
        .clone()
        .unwrap_or_else(|| "codebuddy".to_string());
    UpstreamRegistry::new(map, default)
}

/// 把配置转成前端可编辑的视图（含当前生效值，文件覆盖 + CLI 兜底合并后）。
pub fn config_view(cfg: &UpstreamsConfig, base: &BaseSettings) -> Value {
    let codebuddy_backend = cfg
        .ov("codebuddy")
        .and_then(|o| o.backend.clone())
        .or_else(|| base.codebuddy_backend.clone());
    let trae_chat_host = cfg
        .ov("trae")
        .and_then(|o| o.chat_host.clone())
        .or_else(|| base.trae_chat_host.clone());
    serde_json::json!({
        "default_upstream": cfg.default_upstream.clone().unwrap_or_else(|| "codebuddy".to_string()),
        "upstreams": {
            "codebuddy": {
                "enabled": cfg.enabled("codebuddy", false),
                "backend": codebuddy_backend,
            },
            "workbuddy": { "enabled": cfg.enabled("workbuddy", false) },
            "trae": {
                "enabled": cfg.enabled("trae", false),
                "chat_host": trae_chat_host,
            },
            "opencode": { "enabled": cfg.enabled("opencode", false) },
        },
    })
}
