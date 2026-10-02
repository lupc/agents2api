//! 登录凭据：定位 auth 文件、读取凭据、token 临近过期时自动刷新并回写。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::wbcrypto;

pub const BACKEND: &str = "https://copilot.tencent.com";
pub const DEFAULT_DOMAIN: &str = "www.codebuddy.cn";
pub const USER_AGENT: &str = "codebuddy2openai/2.0";

// ---------------------------------------------------------------------------
// 平台相关：定位 auth 目录
// ---------------------------------------------------------------------------

pub fn auth_dirs() -> Vec<PathBuf> {
    if let Ok(env_dir) = std::env::var("CODEBUDDY_AUTH_DIR") {
        if !env_dir.is_empty() {
            return vec![PathBuf::from(env_dir)];
        }
    }
    let home = dirs_home();
    if cfg!(target_os = "macos") {
        vec![home
            .join("Library")
            .join("Application Support")
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth")]
    } else if cfg!(target_os = "windows") {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| {
            home.join("AppData").join("Local").to_string_lossy().to_string()
        });
        vec![PathBuf::from(local)
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth")]
    } else {
        let xdg = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".local").join("share"));
        vec![xdg
            .join("CodeBuddyExtension")
            .join("Data")
            .join("Public")
            .join("auth")]
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn find_auth_file() -> Option<PathBuf> {
    for d in auth_dirs() {
        if d.is_dir() {
            let mut files: Vec<PathBuf> = list_info_files(&d);
            if files.is_empty() {
                continue;
            }
            // 优先用无时间戳后缀的 "当前" 文件；否则按 mtime 取最新
            let current = d.join("workbuddy-desktop.info");
            if current.is_file() {
                return Some(current);
            }
            files.sort_by_key(|p| {
                std::cmp::Reverse(
                    p.metadata().and_then(|m| m.modified()).ok().map(|t| {
                        t.duration_since(SystemTime::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0)
                    }),
                )
            });
            return files.into_iter().next();
        }
    }
    None
}

fn list_info_files(d: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file()
                && p.extension().map(|x| x == "info").unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Auth 凭据管理（读 + 自动刷新 + 回写）
// ---------------------------------------------------------------------------

struct CredState {
    cached: Option<Value>,
    mtime: Option<SystemTime>,
}

pub struct CredentialManager {
    path: PathBuf,
    state: Mutex<CredState>,
    http: reqwest::Client,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn value_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

impl CredentialManager {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(CredState {
                cached: None,
                mtime: None,
            }),
            http: reqwest::Client::new(),
        }
    }


    fn read_raw(&self) -> Result<Value, String> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("无法读取 auth 文件 {}: {e}", self.path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("auth 文件 JSON 解析失败: {e}"))
    }

    async fn session(&self, st: &mut CredState) -> Result<Value, String> {
        let mt = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if st.cached.is_none() || mt != st.mtime {
            st.cached = Some(self.read_raw()?);
            st.mtime = mt;
        }
        Ok(st.cached.clone().unwrap())
    }

    fn is_expired(s: &Value) -> bool {
        let expires_at = s
            .get("auth")
            .and_then(|a| a.get("expiresAt"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        // 提前 60s 判定过期
        now_ms() >= expires_at.saturating_sub(60_000)
    }

    async fn build_headers(&self, s: &Value) -> Result<Vec<(String, String)>, String> {
        let auth = s.get("auth").cloned().unwrap_or(json!({}));
        let account = s.get("account").cloned().unwrap_or(json!({}));
        let token = wbcrypto::decrypt_auth_field(auth.get("accessToken")).await?;
        let domain = {
            let d = value_str(auth.get("domain"));
            if d.is_empty() {
                DEFAULT_DOMAIN.to_string()
            } else {
                d
            }
        };
        Ok(vec![
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "application/json".into()),
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-User-Id".into(), value_str(account.get("uid"))),
            (
                "X-Enterprise-Id".into(),
                value_str(account.get("enterpriseId")),
            ),
            ("X-Tenant-Id".into(), value_str(account.get("enterpriseId"))),
            ("X-Domain".into(), domain),
            ("User-Agent".into(), USER_AGENT.into()),
        ])
    }

    /// 调后端刷新 token，写回 auth 文件与缓存。
    async fn refresh(&self, st: &mut CredState) -> Result<(), String> {
        let s = self.session(st).await?;
        let auth = s.get("auth").cloned().unwrap_or(json!({}));
        let mut headers = self.build_headers(&s).await?;
        let refresh_token = wbcrypto::decrypt_auth_field(auth.get("refreshToken")).await?;
        headers.push(("X-Refresh-Token".into(), refresh_token));
        headers.push(("X-Auth-Refresh-Source".into(), "plugin".into()));

        let url = format!("{BACKEND}/v2/plugin/auth/token/refresh");
        let resp = self
            .http
            .post(&url)
            .headers(parse_headers(&headers))
            .json(&json!({}))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| format!("刷新 token 网络失败：{e}"))?;
        let data: Value = resp
            .json()
            .await
            .map_err(|e| format!("刷新 token 响应解析失败：{e}"))?;
        if data.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(format!(
                "刷新 token 失败：{}",
                data.get("msg")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| data.to_string())
            ));
        }
        let new_auth = data
            .get("data")
            .cloned()
            .ok_or_else(|| "刷新 token 失败：响应缺少 data".to_string())?;
        if !new_auth.is_object() {
            return Err("刷新 token 失败：data 不是对象".to_string());
        }
        let mut new_auth = new_auth;
        if value_str(new_auth.get("accessToken")).is_empty() {
            return Err("刷新响应缺少访问令牌".to_string());
        }
        // 继承部分字段（refreshToken 兜底取解密后的旧值，避免加密信封被二次加密）
        if value_str(new_auth.get("refreshToken")).is_empty() {
            new_auth["refreshToken"] =
                json!(wbcrypto::decrypt_auth_field(auth.get("refreshToken")).await?);
        }
        if value_str(new_auth.get("domain")).is_empty() {
            new_auth["domain"] = auth.get("domain").cloned().unwrap_or(Value::Null);
        }
        new_auth["lastRefreshTime"] = json!(now_ms());
        let expires_at = new_auth.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0);
        if expires_at == 0 {
            if let Some(exp_in) = new_auth.get("expiresIn").and_then(|v| v.as_i64()) {
                new_auth["expiresAt"] = json!(now_ms() + exp_in * 1000);
            }
        }
        let refresh_expires_at = new_auth
            .get("refreshExpiresAt")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if refresh_expires_at == 0 {
            if let Some(exp_in) = new_auth.get("refreshExpiresIn").and_then(|v| v.as_i64()) {
                new_auth["refreshExpiresAt"] = json!(now_ms() + exp_in * 1000);
            }
        }
        let mut s_new = s.clone();
        s_new["auth"] = new_auth;
        // issue #23：若原文件为 $wbEncrypted 加密格式，回写前必须重新加密，
        // 否则 WorkBuddy 客户端读到明文会报 integrity 错误、可能重置登录态。
        if wbcrypto::is_encrypted_field(auth.get("accessToken")) {
            let a = s_new
                .pointer("/auth/accessToken")
                .cloned()
                .unwrap_or(Value::Null);
            let pt = wbcrypto::decrypt_auth_field(Some(&a)).await?;
            s_new["auth"]["accessToken"] = wbcrypto::encrypt_auth_field(&pt).await?;
            let r = s_new
                .pointer("/auth/refreshToken")
                .cloned()
                .unwrap_or(Value::Null);
            let pt = wbcrypto::decrypt_auth_field(Some(&r)).await?;
            s_new["auth"]["refreshToken"] = wbcrypto::encrypt_auth_field(&pt).await?;
        }
        // 原子写回
        let tmp = self.path.with_extension("info.tmp");
        let text = serde_json::to_string_pretty(&s_new).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("写回 auth 文件失败: {e}"))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("替换 auth 文件失败: {e}"))?;
        let mt = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        st.cached = Some(s_new);
        st.mtime = mt;
        Ok(())
    }

    /// 返回带最新 token 的后端请求 header；必要时先刷新。
    pub async fn get_headers(&self) -> Result<Vec<(String, String)>, String> {
        let mut st = self.state.lock().await;
        let s = self.session(&mut st).await?;
        if Self::is_expired(&s) {
            self.refresh(&mut st).await?;
        }
        let s = self.session(&mut st).await?;
        self.build_headers(&s).await
    }

    pub async fn summary(&self) -> Result<Value, String> {
        let mut st = self.state.lock().await;
        let s = self.session(&mut st).await?;
        let acct = s.get("account").cloned().unwrap_or(json!({}));
        let auth = s.get("auth").cloned().unwrap_or(json!({}));
        Ok(json!({
            "uid": acct.get("uid"),
            "nickname": acct.get("nickname"),
            "enterpriseName": acct.get("enterpriseName"),
            "token_expires_at": auth.get("expiresAt").cloned().unwrap_or(json!(0)),
            "token_expired": Self::is_expired(&s),
        }))
    }
}

pub(crate) fn parse_headers(headers: &[(String, String)]) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            map.insert(name, val);
        }
    }
    map
}
