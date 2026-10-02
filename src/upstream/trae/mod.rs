//! Trae（字节跳动 AI 编程工具）上游。
//!
//! 与 CodeBuddy 不同，Trae 是**私有的嵌套 JSON + SSE 协议**，不是 OpenAI 兼容：
//! - 鉴权头是 `Authorization: Cloud-IDE-JWT <token>`（非 Bearer）；
//! - 请求体要包装成 `{messages, function, config_name, model, ...}` 且 content 必须是数组；
//! - 响应永远是 SSE，错误可能是 HTTP 200 + 内嵌 error 事件；
//! - 登录态在 `storage.json` 的加密 blob 里（见 [`crypto`]）。
//!
//! 主线端点为 IDE Agent 单请求 `POST {chatHost}/api/agent/v3/llm_utils_chat`。

pub mod auth;
pub mod crypto;
pub mod protocol;

use futures::StreamExt;
use serde_json::Value;

use super::{ByteStream, Upstream, UpstreamRequest};
use auth::{TraeCredential, TraeAuthInfo};
use protocol::TraeEventConverter;

pub use protocol::{DEFAULT_CHAT_HOST, DEFAULT_MODELS};

pub struct TraeUpstream {
    cred: TraeCredential,
    chat_host: String,
    function: String,
}

impl TraeUpstream {
    pub fn new(cred: TraeCredential, chat_host: Option<String>, function: Option<String>) -> Self {
        Self {
            cred,
            chat_host: chat_host.unwrap_or_else(|| {
                std::env::var("TRAE_CHAT_HOST").unwrap_or_else(|_| DEFAULT_CHAT_HOST.to_string())
            }),
            function: function.unwrap_or_else(|| {
                std::env::var("TRAE_FUNCTION")
                    .unwrap_or_else(|_| protocol::DEFAULT_FUNCTION.to_string())
            }),
        }
    }

    async fn info(&self) -> Result<TraeAuthInfo, String> {
        self.cred.info().await
    }
}

#[async_trait::async_trait]
impl Upstream for TraeUpstream {
    fn name(&self) -> &str {
        "trae"
    }

    async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String> {
        let info = self.info().await?;
        if info.is_expired() {
            return Err(format!(
                "Trae 登录态已过期（storage: {}），请重新打开 Trae 登录后再试",
                info.storage_path.display()
            ));
        }
        let body = protocol::build_request_body(chat, &self.function);
        let headers = protocol::build_headers(&info, true);
        Ok(UpstreamRequest {
            url: format!("{}/api/agent/v3/llm_utils_chat", self.chat_host),
            headers,
            body,
        })
    }

    /// Trae 的私有事件流 → 标准 OpenAI chunk 流。
    fn translate_stream(&self, raw: ByteStream) -> ByteStream {
        Box::pin(async_stream::stream! {
            let mut converter = TraeEventConverter::new("trae");
            let mut buf: Vec<u8> = Vec::new();
            let mut raw = raw;
            let mut raw_log = String::new();
            while let Some(chunk) = raw.next().await {
                match chunk {
                    Ok(b) => {
                        if raw_log.len() < 8000 {
                            raw_log.push_str(&String::from_utf8_lossy(&b));
                        }
                        buf.extend_from_slice(&b);
                        while let Some(pos) = buf.iter().position(|&c| c == b'\n') {
                            let line: Vec<u8> = buf.drain(..=pos).collect();
                            let mut s = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
                            if s.ends_with('\r') {
                                s.pop();
                            }
                            let out = converter.feed_line(&s);
                            if !out.is_empty() {
                                yield Ok(bytes::Bytes::from(out));
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            if !buf.is_empty() {
                let s = String::from_utf8_lossy(&buf).to_string();
                let out = converter.feed_line(&s);
                if !out.is_empty() {
                    yield Ok(bytes::Bytes::from(out));
                }
            }
            let tail = converter.finish();
            if !tail.is_empty() {
                yield Ok(bytes::Bytes::from(tail));
            }
            crate::config::log(&format!("── TRAE RAW SSE ──\n{raw_log}"));
        })
    }

    /// Trae 统一走流式，聚合由标准 OpenAI 聚合器完成（转换后已是 OpenAI chunk）。
    fn aggregate(&self, raw: &[u8]) -> Value {
        super::aggregate_openai_sse(raw)
    }

    async fn models(&self) -> Vec<String> {
        DEFAULT_MODELS.iter().map(|s| s.to_string()).collect()
    }

    async fn health_info(&self) -> Value {
        let path = self
            .cred
            .storage_path()
            .to_string_lossy()
            .to_string();
        match self.info().await {
            Ok(info) => serde_json::json!({
                "credential": {
                    "storage_file": path,
                    "variant": info.host_variant,
                    "uid": info.user_id,
                    "token_expired": info.is_expired(),
                    "chat_host": self.chat_host,
                }
            }),
            Err(e) => serde_json::json!({"credential_error": e, "storage_file": path}),
        }
    }
}
