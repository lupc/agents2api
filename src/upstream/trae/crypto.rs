//! Trae 登录态（`storage.json`）解密。
//!
//! Trae 不自带明文 auth 文件，登录态是 `storage.json` 里 `iCubeAuthInfo://icube.cloudide`
//! 键下的一个加密 blob。blob 结构：
//!
//! ```text
//! [6B header][32B random][N B AES-128-CBC ciphertext]
//! ```
//!
//! - header `74 63 05 10 00 00`（"tc\5\x10\0\0"）→ AES
//! - header `12 39 20 20 02 03` → AES_PRIVATE
//! - 值以 `{` 开头 → 已是明文 JSON，直接解析
//!
//! 密钥派生：
//! ```text
//! salt  = SALT_A XOR SALT_B   （AES_PRIVATE 用 SALT_C XOR SALT_D）
//! key   = SHA512( SHA512(random) || salt )[0..16]
//! iv    = SHA512( SHA512(random) || salt )[16..32]
//! ```
//!
//! 明文 = `SHA512(body)`(64B 校验和) || PKCS7 填充的 JSON body。
//!
//! salt 常量来自社区逆向实现（autumnsentiment/Trae2api-cn 与 wangqi233/trae2api 一致），
//! 可用 `TRAE_SALT_A`..`TRAE_SALT_D` 环境变量覆盖（逗号分隔的十进制字节）。

use aes::cipher::{BlockDecryptMut, KeyIvInit};
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha512};
use std::sync::LazyLock;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

const HEADER_SIZE: usize = 6;
const RANDOM_BYTES_LEN: usize = 32;
const HASH_SIZE: usize = 64;

const HEADER_AES: [u8; 6] = [0x74, 0x63, 0x05, 0x10, 0x00, 0x00];
const HEADER_AES_PRIVATE: [u8; 6] = [18, 57, 32, 32, 2, 3];

const SALT_A: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109,
    139, 209, 37,
];
const SALT_B: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25,
    181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176, 200,
    235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33,
    12, 125,
];
const SALT_C: [u8; 64] = [
    191, 192, 216, 250, 122, 246, 220, 97, 31, 254, 98, 27, 8, 72, 71, 176, 135, 99, 96, 18, 127,
    101, 203, 104, 211, 102, 191, 125, 37, 72, 150, 156, 51, 229, 121, 35, 17, 153, 141, 177, 110,
    131, 150, 128, 172, 255, 254, 6, 18, 140, 55, 62, 236, 249, 135, 64, 135, 12, 117, 4, 89, 149,
    168, 209,
];
const SALT_D: [u8; 64] = [
    246, 204, 26, 232, 232, 70, 129, 109, 223, 146, 169, 242, 23, 241, 105, 145, 50, 196, 165,
    42, 254, 120, 3, 54, 244, 207, 209, 85, 53, 6, 138, 106, 175, 148, 31, 204, 186, 186, 165,
    182, 87, 142, 49, 10, 39, 110, 26, 154, 86, 56, 173, 125, 18, 64, 198, 225, 99, 99, 83, 82,
    191, 134, 76, 170,
];

/// 允许用环境变量覆盖的 salt（逗号分隔十进制字节），否则用内置常量。
static SALT_A_EFF: LazyLock<[u8; 64]> = LazyLock::new(|| salt_from_env("TRAE_SALT_A", SALT_A));
static SALT_B_EFF: LazyLock<[u8; 64]> = LazyLock::new(|| salt_from_env("TRAE_SALT_B", SALT_B));
static SALT_C_EFF: LazyLock<[u8; 64]> = LazyLock::new(|| salt_from_env("TRAE_SALT_C", SALT_C));
static SALT_D_EFF: LazyLock<[u8; 64]> = LazyLock::new(|| salt_from_env("TRAE_SALT_D", SALT_D));

fn salt_from_env(var: &str, fallback: [u8; 64]) -> [u8; 64] {
    let Ok(raw) = std::env::var(var) else {
        return fallback;
    };
    let parsed: Option<Vec<u8>> = raw
        .split(',')
        .map(|s| s.trim().parse::<u8>().ok())
        .collect();
    match parsed {
        Some(v) if v.len() == 64 => {
            let mut out = [0u8; 64];
            out.copy_from_slice(&v);
            out
        }
        _ => fallback,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum EncType {
    Aes,
    AesPrivate,
    Unknown,
}

pub fn detect_enc_type(header: &[u8]) -> EncType {
    if header == HEADER_AES {
        EncType::Aes
    } else if header == HEADER_AES_PRIVATE {
        EncType::AesPrivate
    } else {
        EncType::Unknown
    }
}

fn xor_salts(a: &[u8; 64], b: &[u8; 64]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for i in 0..64 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// 解密一个 Trae 加密 blob（已 base64 解码后的原始字节）。
///
/// 返回解密出的 JSON 对象。
pub fn decrypt_blob(blob: &[u8]) -> Result<Value, String> {
    if blob.len() < HEADER_SIZE + RANDOM_BYTES_LEN {
        return Err(format!("blob 长度不足：{}", blob.len()));
    }
    let header = &blob[..HEADER_SIZE];
    let enc_type = detect_enc_type(header);
    if enc_type == EncType::Unknown {
        return Err(format!("未知的加密头：{header:02x?}"));
    }
    let random = &blob[HEADER_SIZE..HEADER_SIZE + RANDOM_BYTES_LEN];
    let ciphertext = &blob[HEADER_SIZE + RANDOM_BYTES_LEN..];
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(16) {
        return Err(format!("密文长度非法：{}", ciphertext.len()));
    }

    let salt = match enc_type {
        EncType::AesPrivate => xor_salts(&SALT_C_EFF, &SALT_D_EFF),
        _ => xor_salts(&SALT_A_EFF, &SALT_B_EFF),
    };

    // key = SHA512(SHA512(random) || salt)[0..16], iv = [16..32]
    let hash_of_random = Sha512::digest(random);
    let mut hasher = Sha512::new();
    hasher.update(hash_of_random);
    hasher.update(salt);
    let final_hash = hasher.finalize();
    let key = &final_hash[0..16];
    let iv = &final_hash[16..32];

    let mut buf = ciphertext.to_vec();
    let cipher = Aes128CbcDec::new(key.into(), iv.into());
    let plaintext = cipher
        .decrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut buf)
        .map_err(|e| format!("AES-128-CBC 解密失败：{e}"))?;

    if plaintext.len() < HASH_SIZE {
        return Err(format!("解密结果过短：{}", plaintext.len()));
    }
    let stored_hash = &plaintext[..HASH_SIZE];
    let body = &plaintext[HASH_SIZE..];
    let actual_hash = Sha512::digest(body);
    if actual_hash.as_slice() != stored_hash {
        return Err("解密校验和（SHA-512）不匹配".to_string());
    }
    serde_json::from_slice::<Value>(body).map_err(|e| format!("解密结果不是合法 JSON：{e}"))
}

/// 解密 `storage.json` 中某个字符串值：
/// - 以 `{` 开头 → 明文 JSON，直接解析
/// - 否则 base64 解码后按加密 blob 处理
pub fn decrypt_storage_value(value: &str) -> Result<Value, String> {
    let trimmed = value.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str::<Value>(value)
            .map_err(|e| format!("明文 JSON 解析失败：{e}"));
    }
    let blob = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|e| format!("base64 解码失败：{e}"))?;
    decrypt_blob(&blob)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    /// 用与解密相反的过程构造一个合法 blob，验证解密能还原。
    fn make_blob(body: &[u8], enc_type: &EncType) -> Vec<u8> {
        let salt = match enc_type {
            EncType::AesPrivate => xor_salts(&SALT_C, &SALT_D),
            _ => xor_salts(&SALT_A, &SALT_B),
        };
        let random: Vec<u8> = (0u8..32).collect();
        let hash_of_random = Sha512::digest(&random);
        let mut hasher = Sha512::new();
        hasher.update(hash_of_random);
        hasher.update(salt);
        let final_hash = hasher.finalize();
        let key = &final_hash[0..16];
        let iv = &final_hash[16..32];

        // 明文 = SHA512(body) || body，再整体 PKCS7 填充
        let mut plain = Sha512::digest(body).to_vec();
        plain.extend_from_slice(body);
        // 预留一个 block 的填充空间
        let msg_len = plain.len();
        plain.resize(msg_len + 16, 0);
        let cipher = Aes128CbcEnc::new(key.into(), iv.into());
        let ciphertext = cipher
            .encrypt_padded_mut::<aes::cipher::block_padding::Pkcs7>(&mut plain, msg_len)
            .unwrap()
            .to_vec();

        let mut blob = Vec::new();
        blob.extend_from_slice(match enc_type {
            EncType::AesPrivate => &HEADER_AES_PRIVATE,
            _ => &HEADER_AES,
        });
        blob.extend_from_slice(&random);
        blob.extend_from_slice(&ciphertext);
        blob
    }

    #[test]
    fn test_detect_enc_type() {
        assert_eq!(detect_enc_type(&HEADER_AES), EncType::Aes);
        assert_eq!(detect_enc_type(&HEADER_AES_PRIVATE), EncType::AesPrivate);
        assert_eq!(detect_enc_type(&[0, 1, 2, 3, 4, 5]), EncType::Unknown);
    }

    #[test]
    fn test_decrypt_roundtrip_aes() {
        let body = br#"{"token":"jwt-abc","userId":"u1"}"#;
        let blob = make_blob(body, &EncType::Aes);
        let out = decrypt_blob(&blob).unwrap();
        assert_eq!(out["token"], "jwt-abc");
        assert_eq!(out["userId"], "u1");
    }

    #[test]
    fn test_decrypt_roundtrip_aes_private() {
        let body = br#"{"token":"jwt-xyz"}"#;
        let blob = make_blob(body, &EncType::AesPrivate);
        let out = decrypt_blob(&blob).unwrap();
        assert_eq!(out["token"], "jwt-xyz");
    }

    #[test]
    fn test_decrypt_rejects_tampered_body() {
        let body = br#"{"token":"jwt-abc"}"#;
        let mut blob = make_blob(body, &EncType::Aes);
        // 篡改最后一个密文块，校验和应不匹配
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(decrypt_blob(&blob).is_err());
    }

    #[test]
    fn test_decrypt_plaintext_value() {
        let out = decrypt_storage_value(r#"{"token":"plain"}"#).unwrap();
        assert_eq!(out["token"], "plain");
    }

    #[test]
    fn test_decrypt_base64_encoded_blob() {
        let body = br#"{"token":"b64"}"#;
        let blob = make_blob(body, &EncType::Aes);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&blob);
        let out = decrypt_storage_value(&b64).unwrap();
        assert_eq!(out["token"], "b64");
    }

    #[test]
    fn test_decrypt_rejects_short_blob() {
        assert!(decrypt_blob(&[0u8; 10]).is_err());
    }
}
