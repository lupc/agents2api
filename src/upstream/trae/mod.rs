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
pub mod catalog;
pub mod crypto;
pub mod protocol;

use futures::StreamExt;
use serde_json::Value;

use super::{ByteStream, ModelInfo, Upstream, UpstreamRequest};
use auth::{TraeCredential, TraeAuthInfo};
use protocol::TraeEventConverter;

pub use protocol::{DEFAULT_CHAT_HOST, DEFAULT_MODELS};

pub struct TraeUpstream {
    cred: TraeCredential,
    chat_host: String,
}

impl TraeUpstream {
    pub fn new(cred: TraeCredential, chat_host: Option<String>) -> Self {
        Self {
            cred,
            chat_host: chat_host.unwrap_or_else(|| {
                std::env::var("TRAE_CHAT_HOST").unwrap_or_else(|_| DEFAULT_CHAT_HOST.to_string())
            }),
        }
    }

    async fn info(&self) -> Result<TraeAuthInfo, String> {
        self.cred.info().await
    }

    /// 从 Trae 模型网关拉取真实模型目录（batch_get_detail_param）。
    ///
    /// 请求形态 = 客户端 Agent 聊天选择器的真实请求（参考实现 2026-09-19 抓包固化）：
    /// 7 函数 + access_type=0；头需 `x-bridge-transport: aha`。
    async fn fetch_catalog_raw(&self) -> Result<Value, String> {
        let info = self.info().await?;
        let host = std::env::var("TRAE_MODEL_API_HOST")
            .unwrap_or_else(|_| "https://api5-normal.mchost.guru".to_string());
        let url = format!("{host}/api/ide/v1/batch_get_detail_param");
        let body = serde_json::json!({
            "functions": [
                "builder", "builder_v3", "chat_v3", "code_reviewer",
                "code_review_summary", "refactor", "solo_agent"
            ],
            "agent_type": "solo_agent",
            "current_config_info": { "config_name": "", "is_custom_model": false },
            "mode_type": 0,
            "access_type": 0,
            "ab_force_vids": "",
            "ab_autotest_advanced_mode": 0,
            "show_custom_model": true,
        });
        let mut headers = protocol::build_headers(&info, false);
        headers.push(("x-bridge-transport".to_string(), "aha".to_string()));
        headers.push(("Accept".to_string(), "application/json".to_string()));

        let client = reqwest::Client::new();
        let mut req = client.post(&url).json(&body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("请求失败: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("读取响应失败: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                text.chars().take(200).collect::<String>()
            ));
        }
        serde_json::from_str(&text).map_err(|e| {
            let head: String = text.chars().take(300).collect();
            format!("响应解析失败: {e}；原始响应开头: {head:?}")
        })
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
        let body = protocol::build_request_body(chat, &info);
        let headers = protocol::build_headers(&info, true);
        Ok(UpstreamRequest {
            url: format!("{}{}", self.chat_host, protocol::AGENT_PATH),
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

    /// 聚合收到的 raw 已由 `translate_stream` 转成标准 OpenAI chunk，直接聚合即可。
    fn aggregate(&self, raw: &[u8]) -> Value {
        super::aggregate_openai_sse(raw)
    }

    async fn models(&self) -> Vec<String> {
        // 已同步远程目录时优先消费（客户端可见性口径），否则内置兜底
        if let Some(catalog) = catalog::catalog_snapshot() {
            return catalog.into_iter().map(|m| m.id).collect();
        }
        DEFAULT_MODELS.iter().map(|s| s.to_string()).collect()
    }

    /// 远程目录：batch_get_detail_param 全量解析（倍率/档位/上下文/图片），
    /// 按客户端可见性规则过滤；失败返回 Err 由调用方回退。
    async fn sync_catalog(&self) -> Result<Vec<ModelInfo>, String> {
        let body = self.fetch_catalog_raw().await?;
        let text = serde_json::to_string(&body).map_err(|e| e.to_string())?;
        catalog::parse_official_catalog(&text)
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

