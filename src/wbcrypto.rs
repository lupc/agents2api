//! WorkBuddy 5.6.0+ `$wbEncrypted` 加密登录态支持（对应上游 issue #23）。
//!
//! auth .info 文件中的 accessToken / refreshToken 等字段由明文字符串变为加密信封对象：
//! ```json
//! {"$wbEncrypted": 1, "envelope": "<base64 of JSON>"}
//! ```
//! envelope JSON 结构（suite 1 / AES-256-GCM / sym-v1）：
//! ```json
//! {"suite":1, "keyId":"<16 hex>", "nonce":"<b64 12B>",
//!  "authTag":"<b64 16B>", "ciphertext":"<b64>"}
//! ```
//!
//! 密钥链路：
//! 1. protector key = sha256(atRestSecretKey 字符串, utf8)，32 字节。
//!    atRestSecretKey（44 字符 canonical base64）由 WorkBuddy Electron 原生层提供，
//!    可通过 `ELECTRON_RUN_AS_NODE=1 <WorkBuddy 可执行文件> -e <js>` 离线获取，
//!    或由环境变量 `WORKBUDDY_AT_REST_SECRET` 直接提供。
//! 2. keyId = sha256(protector_key).hex[:16]。
//! 3. field 场景 AAD（sym-v1, framing=field）：见 [`aad_field`]。
//! 4. AES-256-GCM，nonce 12B，authTag 16B，密文 = 明文（UTF-8）。

use std::path::PathBuf;
use std::sync::OnceLock;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 工作目录环境变量指定 WorkBuddy 可执行文件时优先使用。
fn electron_candidates() -> Vec<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(env) = std::env::var("WORKBUDDY_ELECTRON_PATH") {
        if !env.is_empty() {
            cands.push(PathBuf::from(env));
        }
    }
    if cfg!(target_os = "macos") {
        cands.push(PathBuf::from(
            "/Applications/WorkBuddy.app/Contents/MacOS/Electron",
        ));
    } else if cfg!(target_os = "windows") {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| {
            format!(
                "{}\\AppData\\Local",
                std::env::var("USERPROFILE").unwrap_or_default()
            )
        });
        cands.push(PathBuf::from(&local).join("Programs\\WorkBuddy\\WorkBuddy.exe"));
    } else {
        cands.push(PathBuf::from("/opt/WorkBuddy/workbuddy"));
        cands.push(PathBuf::from("/usr/bin/workbuddy"));
    }
    cands.into_iter().filter(|p| p.is_file()).collect()
}

const LOGGERGET_JS: &str = "const b = process._linkedBinding('electron_browser_workbuddy_storage');process.stdout.write(b.loggerGet().toString('utf8'));";

/// 通过 WorkBuddy 自带 Electron（run-as-node）调用原生 loggerGet 获取 key payload。
async fn fetch_key_payload() -> Result<serde_json::Map<String, Value>, String> {
    let cands = electron_candidates();
    if cands.is_empty() {
        return Err(
            "无法获取 WorkBuddy at-rest 密钥（loggerGet）。未找到 WorkBuddy 可执行文件；\
             也可设置 WORKBUDDY_AT_REST_SECRET 环境变量直接提供 atRestSecretKey（44 字符 base64）。"
                .to_string(),
        );
    }
    let mut last_err = String::new();
    for exe in cands {
        let exe_str = exe.display().to_string();
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.arg("-e")
            .arg(LOGGERGET_JS)
            .env("ELECTRON_RUN_AS_NODE", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output()).await {
            Ok(Ok(out)) if out.status.success() && !out.stdout.is_empty() => {
                let text = String::from_utf8_lossy(&out.stdout);
                match serde_json::from_str::<Value>(&text) {
                    Ok(v) => return Ok(v.as_object().cloned().unwrap_or_default()),
                    Err(e) => last_err = format!("{exe_str}: 解析 loggerGet 输出失败: {e}"),
                }
            }
            Ok(Ok(out)) => {
                last_err = format!(
                    "{}: rc={:?} {}",
                    exe_str,
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).chars().take(200).collect::<String>()
                );
            }
            Ok(Err(e)) => last_err = format!("{exe_str}: {e}"),
            Err(_) => last_err = format!("{exe_str}: 超时（20s）"),
        }
    }
    Err(format!(
        "无法获取 WorkBuddy at-rest 密钥（loggerGet）。请确认 WorkBuddy 已安装；\
         也可设置 WORKBUDDY_AT_REST_SECRET 环境变量直接提供 atRestSecretKey（44 字符 base64）。最后错误：{last_err}"
    ))
}

static KEY_CACHE: OnceLock<Option<(Vec<u8>, String)>> = OnceLock::new();
static KEY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 返回 (protector_key 32B, keyId 16hex)。结果缓存；失败时按 env 兜底。
async fn protector_key() -> Result<(Vec<u8>, String), String> {
    if let Some(cache) = KEY_CACHE.get() {
        if let Some(kv) = cache {
            return Ok(kv.clone());
        }
    }
    let _guard = KEY_LOCK.lock().await;
    if let Some(cache) = KEY_CACHE.get() {
        if let Some(kv) = cache {
            return Ok(kv.clone());
        }
    }
    let secret_b64 = match std::env::var("WORKBUDDY_AT_REST_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            let payload = fetch_key_payload()
                .await
                .map_err(|e| e.to_string())?;
            payload
                .get("atRestSecretKey")
                .and_then(|v| v.as_str())
                .ok_or("loggerGet 返回缺少 atRestSecretKey")?
                .to_string()
        }
    };
    let key: [u8; 32] = Sha256::digest(secret_b64.as_bytes()).into();
    let key_id = hex_lower(&Sha256::digest(key)[..8]); // 16 hex chars = 8 bytes
    let kv = (key.to_vec(), key_id);
    let _ = KEY_CACHE.set(Some(kv.clone()));
    Ok(kv)
}

pub(crate) fn hex_lower(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// AAD 构造（sym-v1 field framing）。
fn aad_field(key_id: &str) -> Vec<u8> {
    fn lp(b: &[u8]) -> Vec<u8> {
        let mut v = (b.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(b);
        v
    }
    let mut aad: Vec<u8> = Vec::new();
    aad.extend_from_slice(b"WB-AAD\0");
    aad.push(0x01);
    aad.extend_from_slice(&lp(b"WBEV1")); // field format id
    aad.extend_from_slice(&lp(b"sym-v1")); // scheme
    aad.extend_from_slice(&1u32.to_be_bytes()); // suite
    aad.extend_from_slice(&lp(key_id.as_bytes()));
    aad.push(0x02); // FRAMING_CODE.field
    aad.push(0x00); // sequence = undefined
    aad.push(0x00); // final = undefined
    aad
}

/// 判断字段值是否为 $wbEncrypted 加密信封。
pub fn is_encrypted_field(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Object(m)) => {
            m.get("$wbEncrypted").and_then(|v| v.as_i64()) == Some(1)
                && m.get("envelope").map(|v| v.is_string()).unwrap_or(false)
        }
        _ => false,
    }
}

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("base64 解码失败: {e}"))
}

/// 解密 .info 中的字段值：
/// - 明文字符串（5.6 之前 / 旧版客户端）→ 原样返回；
/// - `{"$wbEncrypted":1,"envelope":...}` → 解密后返回明文字符串。
pub async fn decrypt_auth_field(value: Option<&Value>) -> Result<String, String> {
    if !is_encrypted_field(value) {
        return Ok(match value {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        });
    }
    let obj = value.unwrap().as_object().unwrap();
    let envelope_b64 = obj.get("envelope").and_then(|v| v.as_str()).unwrap_or("");
    let env_val: Value = serde_json::from_slice(&b64_decode(envelope_b64)?)
        .map_err(|e| format!("envelope JSON 解析失败: {e}"))?;
    let suite = env_val.get("suite").and_then(|v| v.as_i64()).unwrap_or(0);
    if suite != 1 {
        return Err(format!("不支持的加密 suite：{suite}"));
    }
    let key_id = env_val
        .get("keyId")
        .and_then(|v| v.as_str())
        .ok_or("envelope 缺少 keyId")?
        .to_string();
    let (_, cur_key_id) = protector_key().await?;
    if key_id != cur_key_id {
        return Err(format!(
            "envelope keyId({key_id}) 与本机派生 keyId({cur_key_id}) 不一致，WorkBuddy 可能已更换密钥，请重新获取"
        ));
    }
    let nonce = b64_decode(env_val.get("nonce").and_then(|v| v.as_str()).unwrap_or(""))?;
    let ciphertext = b64_decode(
        env_val
            .get("ciphertext")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    )?;
    let auth_tag = b64_decode(env_val.get("authTag").and_then(|v| v.as_str()).unwrap_or(""))?;
    if nonce.len() != 12 {
        return Err(format!("nonce 长度异常：{}", nonce.len()));
    }
    let key = protector_key().await?.0;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;
    let mut ct_with_tag = ciphertext;
    ct_with_tag.extend_from_slice(&auth_tag);
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ct_with_tag,
                aad: &aad_field(&key_id),
            },
        )
        .map_err(|_| "AES-GCM 解密失败（AAD 或密钥不匹配）".to_string())?;
    String::from_utf8(plaintext).map_err(|_| "解密结果不是合法 UTF-8".to_string())
}

/// 将明文加密回 $wbEncrypted 信封（用于刷新 token 后安全回写 .info，
/// 保持客户端 5.6+ 可读）。nonce 使用随机 12B。
pub async fn encrypt_auth_field(plaintext: &str) -> Result<Value, String> {
    let (key, key_id) = protector_key().await?;
    use rand::RngCore;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext.as_bytes(),
                aad: &aad_field(&key_id),
            },
        )
        .map_err(|e| e.to_string())?;
    let (ciphertext, auth_tag) = ct.split_at(ct.len() - 16);
    let envelope = serde_json::json!({
        "suite": 1,
        "keyId": key_id,
        "nonce": base64::engine::general_purpose::STANDARD.encode(nonce_bytes),
        "authTag": base64::engine::general_purpose::STANDARD.encode(auth_tag),
        "ciphertext": base64::engine::general_purpose::STANDARD.encode(ciphertext),
    });
    let envelope_b64 = base64::engine::general_purpose::STANDARD
        .encode(envelope.to_string().as_bytes());
    Ok(serde_json::json!({
        "$wbEncrypted": 1,
        "envelope": envelope_b64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无密钥来源时（无 env、无 WorkBuddy 安装）应返回明确错误而不是 panic。
    #[tokio::test]
    async fn test_decrypt_roundtrip_requires_key() {
        let enc = serde_json::json!({"$wbEncrypted": 1, "envelope": "e30="});
        let r = decrypt_auth_field(Some(&enc)).await;
        assert!(r.is_err());
    }

    #[test]
    fn test_is_encrypted_field() {
        assert!(is_encrypted_field(Some(&serde_json::json!(
            {"$wbEncrypted": 1, "envelope": "abc"}
        ))));
        assert!(!is_encrypted_field(Some(&serde_json::json!("plain"))));
        assert!(!is_encrypted_field(None));
    }

    #[test]
    fn test_aad_field_layout() {
        let aad = aad_field("0123456789abcdef");
        assert_eq!(&aad[..7], b"WB-AAD\0");
        assert_eq!(aad[7], 0x01);
        assert_eq!(aad[8], 0); // len(WBEV1) u32 be
        assert_eq!(aad[9], 0);
        assert_eq!(aad[10], 0);
        assert_eq!(aad[11], 5);
        assert_eq!(&aad[12..17], b"WBEV1");
        assert_eq!(&aad[17..21], &[0, 0, 0, 6]);
        assert_eq!(&aad[21..27], b"sym-v1");
        assert_eq!(&aad[27..31], &[0, 0, 0, 1]);
        assert_eq!(&aad[31..35], &[0, 0, 0, 16]);
        assert_eq!(&aad[35..51], b"0123456789abcdef");
        assert_eq!(aad[51], 0x02);
        assert_eq!(&aad[52..], &[0x00, 0x00]);
        assert_eq!(aad.len(), 54);
    }
}
