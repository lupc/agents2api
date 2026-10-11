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

    /// 从 Trae 模型网关拉取真实模型目录。
    ///
    /// 端点与请求头来自参考实现（batch_get_detail_param，需 `x-bridge-transport: aha`）。
    /// 响应结构未完全公开，这里防御性解析：递归收集所有 `config_name` 字符串值；
    /// 任何失败都返回 Err 由调用方回退。
    async fn fetch_remote_models(&self) -> Result<Vec<String>, String> {
        let info = self.info().await?;
        let host = std::env::var("TRAE_MODEL_API_HOST")
            .unwrap_or_else(|_| "https://api5-normal.mchost.guru".to_string());
        let url = format!("{host}/api/ide/v1/batch_get_detail_param");
        let body = serde_json::json!({
            "functions": ["solo_work_lite", "solo_agent"],
            "config_names": null,
            "need_prompt": false,
            "current_config_info": null,
            "poly_prompt": true,
            "mode_type": null,
            "agent_type": null,
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
            .timeout(std::time::Duration::from_secs(15))
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
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| {
                let head: String = text.chars().take(300).collect();
                format!("响应解析失败: {e}；原始响应开头: {head:?}")
            })?;
        let mut names = Vec::new();
        collect_config_names(&v, &mut names);
        names.sort();
        names.dedup();
        // 过滤明显非模型的值
        names.retain(|n| {
            !n.is_empty()
                && !n.starts_with("http")
                && n.len() < 64
        });
        Ok(names)
    }
}

/// 递归收集 JSON 中所有 `config_name` 字符串值（防御性解析，容忍结构漂移）。
fn collect_config_names(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(s)) = m.get("config_name") {
                if !s.is_empty() {
                    out.push(s.clone());
                }
            }
            for val in m.values() {
                collect_config_names(val, out);
            }
        }
        Value::Array(items) => {
            for i in items {
                collect_config_names(i, out);
            }
        }
        _ => {}
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
        DEFAULT_MODELS.iter().map(|s| s.to_string()).collect()
    }

    /// 模型同步：尝试从 Trae 模型网关拉取真实目录（batch_get_detail_param），
    /// 失败或为空则回退内置列表。
    async fn sync_models(&self) -> Vec<String> {
        match self.fetch_remote_models().await {
            Ok(list) if !list.is_empty() => {
                crate::config::log(&format!(
                    "🔄 Trae 远程模型同步成功：{} 个模型",
                    list.len()
                ));
                list
            }
            Err(e) => {
                crate::config::log(&format!("⚠️ Trae 远程模型同步失败，回退内置列表: {e}"));
                self.models().await
            }
            Ok(_) => self.models().await,
        }
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
