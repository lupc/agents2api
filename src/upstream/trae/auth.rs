//! Trae 凭据：定位 `storage.json`、解密取 JWT 与 userId。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use tokio::sync::Mutex;

use super::crypto;

/// storage.json 里存登录态的键。
const STORAGE_KEY: &str = "iCubeAuthInfo://icube.cloudide";
/// 设备 id 键前缀（`iCubeAuthInfo://iCube-dc:<id>`）。
const DEVICE_ID_PREFIX: &str = "iCubeAuthInfo://iCube-dc:";

/// 各变体的 `%APPDATA%/<dir>/User/globalStorage/storage.json` 目录名。
const STORAGE_DIR_VARIANTS: &[&str] = &[
    "Trae CN",
    "TRAE SOLO CN",
    "Trae",
    "TRAE SOLO",
];

fn appdata_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join("AppData").join("Roaming")
        })
}

/// 候选 storage.json 路径（可用 `TRAE_STORAGE_JSON` 直接指定）。
pub fn storage_json_candidates() -> Vec<PathBuf> {
    if let Ok(explicit) = std::env::var("TRAE_STORAGE_JSON") {
        if !explicit.is_empty() {
            return vec![PathBuf::from(explicit)];
        }
    }
    let base = appdata_dir();
    STORAGE_DIR_VARIANTS
        .iter()
        .map(|v| {
            base.join(v)
                .join("User")
                .join("globalStorage")
                .join("storage.json")
        })
        .collect()
}

pub fn find_storage_json() -> Option<PathBuf> {
    storage_json_candidates().into_iter().find(|p| p.is_file())
}

/// 解密后的 Trae 账号信息。
#[derive(Clone, Debug)]
pub struct TraeAuthInfo {
    pub token: String,
    pub user_id: String,
    pub device_id: String,
    pub machine_id: String,
    /// 过期时间（毫秒，0 表示未知）。
    pub expires_at_ms: i64,
    pub host_variant: String,
    pub storage_path: PathBuf,
}

impl TraeAuthInfo {
    pub fn is_expired(&self) -> bool {
        if self.expires_at_ms <= 0 {
            return false; // 未知则不主动判定
        }
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        now_ms >= self.expires_at_ms
    }
}

fn value_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 从解析后的 storage.json 提取 token 等字段。
fn extract_auth(root: &Value, path: &Path, variant: &str) -> Result<TraeAuthInfo, String> {
    let raw = root
        .get(STORAGE_KEY)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("storage.json 缺少键 {STORAGE_KEY}"))?;
    let auth = crypto::decrypt_storage_value(raw)
        .map_err(|e| format!("解密 {STORAGE_KEY} 失败：{e}"))?;

    let token = value_str(auth.get("token"));
    if token.is_empty() {
        return Err("解密结果缺少 token".to_string());
    }

    // 过期时间字段有多种命名；> 1e12 视为毫秒
    let mut expires_at_ms = 0i64;
    for key in ["expiredAt", "TokenExpireAt", "tokenExpireAt"] {
        if let Some(v) = auth.get(key).and_then(|v| v.as_i64()) {
            if v > 0 {
                expires_at_ms = if v > 1_000_000_000_000 { v } else { v * 1000 };
                break;
            }
        }
    }

    // 设备 id：storage.json 顶层的 `iCubeAuthInfo://iCube-dc:<id>` 键
    let mut device_id = String::new();
    if let Some(obj) = root.as_object() {
        for key in obj.keys() {
            if let Some(rest) = key.strip_prefix(DEVICE_ID_PREFIX) {
                device_id = rest.to_string();
                break;
            }
        }
    }
    let machine_id = value_str(root.pointer("/telemetry/machineId"));

    Ok(TraeAuthInfo {
        token,
        user_id: value_str(auth.get("userId")),
        device_id,
        machine_id,
        expires_at_ms,
        host_variant: variant.to_string(),
        storage_path: path.to_path_buf(),
    })
}

/// 缓存状态。
struct Cache {
    info: Option<TraeAuthInfo>,
    mtime: Option<SystemTime>,
}

/// Trae 凭据管理器：按 mtime 失效缓存，**只读不刷新**
/// （Trae 的 token 刷新与客户端登录可能互斥，交回客户端处理）。
pub struct TraeCredential {
    path: PathBuf,
    variant: String,
    cache: Mutex<Cache>,
}

impl TraeCredential {
    pub fn new(path: PathBuf) -> Self {
        let variant = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        Self {
            path,
            variant,
            cache: Mutex::new(Cache {
                info: None,
                mtime: None,
            }),
        }
    }

    pub fn storage_path(&self) -> &Path {
        &self.path
    }

    /// 读取并解密登录态（带 mtime 缓存）。
    pub async fn info(&self) -> Result<TraeAuthInfo, String> {
        let mut cache = self.cache.lock().await;
        let mt = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if cache.info.is_none() || mt != cache.mtime {
            let text = std::fs::read_to_string(&self.path)
                .map_err(|e| format!("无法读取 {}: {e}", self.path.display()))?;
            let root: Value = serde_json::from_str(&text)
                .map_err(|e| format!("storage.json 解析失败：{e}"))?;
            let info = extract_auth(&root, &self.path, &self.variant)?;
            cache.info = Some(info);
            cache.mtime = mt;
        }
        Ok(cache.info.clone().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_auth_plaintext() {
        let root = json!({
            STORAGE_KEY: r#"{"token":"jwt-1","userId":"u9","expiredAt":2000000000000}"#,
            "iCubeAuthInfo://iCube-dc:12345": {"x": 1},
            "telemetry": {"machineId": "m-abc"},
        });
        let info = extract_auth(&root, Path::new("test.json"), "Trae CN").unwrap();
        assert_eq!(info.token, "jwt-1");
        assert_eq!(info.user_id, "u9");
        assert_eq!(info.device_id, "12345");
        assert_eq!(info.machine_id, "m-abc");
        assert_eq!(info.expires_at_ms, 2000000000000);
        assert!(!info.is_expired());
    }

    #[test]
    fn test_extract_auth_seconds_normalized() {
        let root = json!({ STORAGE_KEY: r#"{"token":"t","TokenExpireAt":1700000000}"# });
        let info = extract_auth(&root, Path::new("t.json"), "Trae").unwrap();
        assert_eq!(info.expires_at_ms, 1_700_000_000_000);
        assert!(info.is_expired());
    }

    #[test]
    fn test_extract_auth_missing_key() {
        let root = json!({"other": "x"});
        assert!(extract_auth(&root, Path::new("t.json"), "Trae").is_err());
    }

    #[test]
    fn test_extract_auth_missing_token() {
        let root = json!({ STORAGE_KEY: r#"{"userId":"u"}"# });
        assert!(extract_auth(&root, Path::new("t.json"), "Trae").is_err());
    }
}
