//! OpenCode Zen 匿名免费通道上游。
//!
//! 原理：Zen 的免费模型（id 含 `-free`）并非完全开放，而是把校验从「用户账户」
//! 转移到「客户端身份」上。若请求不被识别为「来自 OpenCode 编码代理」，网关会对
//! 免费模型返回：
//! ```json
//! 403 {"type":"error","error":{"type":"FreeTierError",
//!      "message":"OpenCode's free tier can only be used from within OpenCode"}}
//! ```
//! 通过条件（据官方 CLI 反编译 + 本机实测校准）：
//! - 固定公共哨兵密钥：`Authorization: Bearer public`；
//! - 客户端身份头：`user-agent: opencode/<版本>`、`x-opencode-client: cli`、
//!   `x-opencode-project`、`x-opencode-request`；
//! - 会话关联头：`x-opencode-session` / `x-opencode-session-id` / `x-session-affinity`
//!   必须是 `ses_<12hex><14base62>` 形态——其中 12 位 hex 是会话真实
//!   `createdAt` 毫秒时间戳（服务端会反解回时间），后 14 位 base62 为随机后缀；
//! - **请求体必须 `stream: true`，且 `tools` 同时包含 `read` 与 `bash` 两个
//!   function schema**（缺任一 → `400`/`403`；额外工具不受影响）。这是免费层的
//!   代理身份门槛——它要求请求看起来像带文件/命令工具的编码代理。
//!
//! 端点：OpenAI 兼容的 `https://opencode.ai/zen/v1/chat/completions`；
//! `prepare_chat` 直通 body 并补齐上述门槛字段；`translate_stream` 恒等转发。

use serde_json::Value;

use super::{passthrough_stream, ByteStream, Upstream, UpstreamRequest};

/// Zen 网关 OpenAI 兼容端点（不含方法路径）。
pub const ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";

/// 匿名通道的公共哨兵密钥（官方客户端未配置 key 时发送的固定值）。
pub const PUBLIC_TOKEN: &str = "public";

/// 伪装成 OpenCode CLI 的 user-agent（网关据此判定客户端身份）。
/// 版本号跟随官方 CLI；过旧可能触发 Cloudflare WAF 挑战。
pub const OPENCODE_UA: &str = "opencode/1.19.0";

/// 免费模型 id 的判定标记（Zen 以 `-free` 后缀标注免费池）。
pub const FREE_SUFFIX: &str = "-free";

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// n 个 base62 随机字符。
fn base62(n: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter()
        .map(|b| BASE62[(*b as usize) % BASE62.len()] as char)
        .collect()
}

/// 生成形如 `ses_<12hex><14base62>` 的会话 id。
///
/// 前 12 位 hex 是当前毫秒时间戳（服务端反解为会话 createdAt），
/// 因此既能通过格式校验，又带有真实时间语义。
fn new_session_id() -> String {
    let ms = chrono::Local::now().timestamp_millis() as u64 & 0xFFFF_FFFF_FFFF;
    format!("ses_{ms:012x}{}", base62(14))
}

/// 每请求一个关联 id（服务端用于请求级关联，无需跨请求稳定）。
fn new_request_id() -> String {
    format!("req_{}", base62(20))
}

/// 免费层门槛：`read` / `bash` 两个必需工具的 function schema 骨架。
/// 网关要求请求形如「带文件/命令工具的编码代理」，缺失即拒。
const READ_TOOL_NAME: &str = "read";
const BASH_TOOL_NAME: &str = "bash";

fn gate_tool(name: &str, desc: &str, arg: &str, arg_desc: &str) -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": name,
            "description": desc,
            "parameters": {
                "type": "object",
                "properties": { arg: {"type": "string", "description": arg_desc} },
                "required": [arg]
            }
        }
    })
}

/// 确保 body 满足免费层门槛：`stream: true` 且 tools 同时含 `read` 与 `bash`。
///
/// 已有同名工具则保留客户端定义；缺失则补一个最小 schema 骨架。
/// 额外工具原样保留（实测不受影响）。
fn enforce_free_tier_gate(body: &Value) -> Value {
    let mut out = body.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    obj.insert("stream".to_string(), Value::Bool(true));

    let tools = obj
        .entry("tools".to_string())
        .or_insert_with(|| Value::Array(vec![]));
    if !tools.is_array() {
        *tools = Value::Array(vec![]);
    }
    let arr = tools.as_array_mut().unwrap();
    let has_tool = |arr: &[Value], name: &str| -> bool {
        arr.iter().any(|t| {
            t.pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(|n| n == name)
                .unwrap_or(false)
        })
    };
    let has_read = has_tool(arr, READ_TOOL_NAME);
    let has_bash = has_tool(arr, BASH_TOOL_NAME);
    if !has_read {
        arr.push(gate_tool(
            READ_TOOL_NAME,
            "Read a file from the filesystem",
            "filePath",
            "Absolute or relative path of the file to read",
        ));
    }
    if !has_bash {
        arr.push(gate_tool(
            BASH_TOOL_NAME,
            "Execute a shell command",
            "command",
            "The shell command to execute",
        ));
    }
    out
}

/// 从 `/models` 响应里筛出免费模型（id 含 `-free`）。
fn parse_free_models(v: &Value) -> Vec<String> {
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                .filter(|id| id.to_ascii_lowercase().contains(FREE_SUFFIX))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// OpenCode Zen 匿名免费通道上游。
pub struct OpenCodeZenFreeUpstream {
    /// 上游标识（供 `/{name}/v1/*` 路由与后台列举）。
    name: String,
    base_url: String,
    /// 匿名哨兵密钥（默认 `public`，可用环境变量覆盖）。
    token: String,
    /// CLI user-agent（可用环境变量覆盖版本）。
    user_agent: String,
    /// 会话 id：进程内稳定（服务端据此关联同一客户端并复用 KV 缓存）。
    session_id: String,
    /// 项目归属：绑定控制台用量分组；默认 `global`。
    project_id: String,
}

impl OpenCodeZenFreeUpstream {
    pub fn new(name: String, base_url: Option<String>) -> Self {
        let token = std::env::var("OPENCODE_ZEN_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| PUBLIC_TOKEN.to_string());
        let user_agent = std::env::var("OPENCODE_ZEN_UA")
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| OPENCODE_UA.to_string());
        let project_id = std::env::var("OPENCODE_ZEN_PROJECT")
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "global".to_string());
        Self {
            name,
            base_url: base_url
                .unwrap_or_else(|| ZEN_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            token,
            user_agent,
            session_id: new_session_id(),
            project_id,
        }
    }

    /// 客户端身份 + 会话关联头（缺失或格式不符会被网关判定为匿名 SDK 调用 → 403）。
    fn identity_headers(&self) -> Vec<(String, String)> {
        vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.token),
            ),
            ("user-agent".to_string(), self.user_agent.clone()),
            ("x-opencode-client".to_string(), "cli".to_string()),
            ("x-opencode-project".to_string(), self.project_id.clone()),
            // 会话关联三件套：带 ses_ 前缀 + 时间戳的 id
            ("x-opencode-session".to_string(), self.session_id.clone()),
            (
                "x-opencode-session-id".to_string(),
                self.session_id.clone(),
            ),
            ("x-session-affinity".to_string(), self.session_id.clone()),
            ("x-session-id".to_string(), self.session_id.clone()),
        ]
    }
}

#[async_trait::async_trait]
impl Upstream for OpenCodeZenFreeUpstream {
    fn name(&self) -> &str {
        &self.name
    }

    async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String> {
        let mut headers = self.identity_headers();
        // 请求级关联 id（每次请求不同，对齐 CLI 行为）
        headers.push(("x-opencode-request".to_string(), new_request_id()));
        Ok(UpstreamRequest {
            url: format!("{}/chat/completions", self.base_url),
            headers,
            // 补齐免费层门槛（stream + read/bash tools），否则网关 403
            body: enforce_free_tier_gate(chat),
        })
    }

    fn translate_stream(&self, raw: ByteStream) -> ByteStream {
        passthrough_stream(raw)
    }

    fn aggregate(&self, raw: &[u8]) -> Value {
        super::super::aggregate_openai_sse(raw)
    }

    /// 匿名拉取 `/models`，仅保留免费池（id 含 `-free`）。
    async fn models(&self) -> Vec<String> {
        let client = match reqwest::Client::builder().build() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let mut req = client.get(format!("{}/models", self.base_url));
        for (k, v) in self.identity_headers() {
            req = req.header(k, v);
        }
        let Ok(resp) = req.send().await else {
            return Vec::new();
        };
        let Ok(json) = resp.json::<Value>().await else {
            return Vec::new();
        };
        parse_free_models(&json)
    }

    async fn health_info(&self) -> Value {
        serde_json::json!({
            "credential": {
                "source": "opencode zen anonymous free lane",
                "endpoint": self.base_url,
                "token": self.token,
                "user_agent": self.user_agent,
                "session_id": self.session_id,
                "project": self.project_id,
                "note": "匿名免费通道：仅 -free 模型可用，身份校验基于客户端标识头",
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_free_models_filters_suffix() {
        let v = json!({"data": [
            {"id": "nemotron-3.5-lightning-free"},
            {"id": "glm-5.2"},
            {"id": "deepseek-v4-flash-FREE"},
            {"id": "kimi-k3"},
        ]});
        assert_eq!(
            parse_free_models(&v),
            vec![
                "nemotron-3.5-lightning-free".to_string(),
                "deepseek-v4-flash-FREE".to_string(),
            ]
        );
    }

    #[test]
    fn test_parse_free_models_empty_when_missing() {
        assert!(parse_free_models(&json!({})).is_empty());
        assert!(parse_free_models(&json!({"data": []})).is_empty());
    }

    #[test]
    fn test_session_id_shape() {
        let id = new_session_id();
        // ses_ + 12hex + 14base62 = 4 + 26
        assert_eq!(id.len(), 30, "unexpected length: {id}");
        assert!(id.starts_with("ses_"));
        let body = &id[4..];
        assert!(
            body[..12].chars().all(|c| c.is_ascii_hexdigit()),
            "first 12 must be hex: {id}"
        );
        assert!(
            body[12..].chars().all(|c| c.is_ascii_alphanumeric()),
            "suffix must be base62-ish: {id}"
        );
    }

    #[test]
    fn test_session_id_encodes_timestamp() {
        // 前 12 位 hex 应能反解出一个合理的近期毫秒时间戳
        let id = new_session_id();
        let ts = u64::from_str_radix(&id[4..16], 16).unwrap();
        let now = chrono::Local::now().timestamp_millis() as u64;
        assert!(ts <= now && now - ts < 60_000, "ts={ts} now={now}");
    }

    #[tokio::test]
    async fn test_prepare_chat_injects_identity_and_sentinel() {
        let u = OpenCodeZenFreeUpstream::new("opencode-free".to_string(), None);
        let req = u
            .prepare_chat(&json!({"model": "nemotron-3.5-lightning-free", "messages": []}))
            .await
            .unwrap();
        let map: std::collections::HashMap<_, _> = req.headers.iter().cloned().collect();
        assert_eq!(map.get("Authorization").unwrap(), "Bearer public");
        assert_eq!(map.get("user-agent").unwrap(), OPENCODE_UA);
        assert_eq!(map.get("x-opencode-client").unwrap(), "cli");
        assert_eq!(map.get("x-opencode-project").unwrap(), "global");
        // 会话关联三件套齐全且一致
        let session = map.get("x-opencode-session").unwrap();
        assert_eq!(map.get("x-opencode-session-id").unwrap(), session);
        assert_eq!(map.get("x-session-affinity").unwrap(), session);
        assert!(session.starts_with("ses_"));
        assert!(map.get("x-opencode-request").is_some());
        assert_eq!(req.url, "https://opencode.ai/zen/v1/chat/completions");
        assert_eq!(req.body["model"], "nemotron-3.5-lightning-free");
    }

    #[test]
    fn test_gate_adds_read_and_bash_and_stream() {
        let body = json!({"model": "m", "messages": [], "stream": false});
        let out = enforce_free_tier_gate(&body);
        assert_eq!(out["stream"], true);
        let names: Vec<&str> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"bash"));
    }

    #[test]
    fn test_gate_preserves_client_tool_definitions() {
        let body = json!({
            "model": "m", "messages": [],
            "tools": [
                {"type": "function", "function": {"name": "read", "description": "my custom read"}},
                {"type": "function", "function": {"name": "bash", "description": "my custom bash"}},
            ]
        });
        let out = enforce_free_tier_gate(&body);
        let arr = out["tools"].as_array().unwrap();
        // 只有两个，未重复追加
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["function"]["description"], "my custom read");
        assert_eq!(arr[1]["function"]["description"], "my custom bash");
    }

    #[test]
    fn test_gate_keeps_extra_tools() {
        let body = json!({
            "model": "m", "messages": [],
            "tools": [{"type": "function", "function": {"name": "get_weather"}}]
        });
        let out = enforce_free_tier_gate(&body);
        let names: Vec<&str> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"get_weather"));
        assert!(names.contains(&"read"));
        assert!(names.contains(&"bash"));
    }

    #[tokio::test]
    async fn test_prepare_chat_enforces_gate() {
        let u = OpenCodeZenFreeUpstream::new("opencode-free".to_string(), None);
        let req = u
            .prepare_chat(&json!({"model": "m", "messages": [], "stream": false}))
            .await
            .unwrap();
        assert_eq!(req.body["stream"], true);
        let names: Vec<&str> = req.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"read") && names.contains(&"bash"));
    }

    #[test]
    fn test_session_stable_across_requests() {
        let u = OpenCodeZenFreeUpstream::new("opencode-free".to_string(), None);
        let h1: std::collections::HashMap<_, _> = u.identity_headers().into_iter().collect();
        let h2: std::collections::HashMap<_, _> = u.identity_headers().into_iter().collect();
        assert_eq!(h1.get("x-opencode-session"), h2.get("x-opencode-session"));
    }

    #[test]
    fn test_request_id_differs_per_call() {
        assert_ne!(new_request_id(), new_request_id());
    }

    #[tokio::test]
    async fn test_custom_base_url_trimmed() {
        let u = OpenCodeZenFreeUpstream::new(
            "opencode-free".to_string(),
            Some("https://mirror.example/zen/v1/".to_string()),
        );
        let req = u.prepare_chat(&json!({"messages": []})).await.unwrap();
        assert_eq!(req.url, "https://mirror.example/zen/v1/chat/completions");
    }
}
