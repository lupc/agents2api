//! CodeBuddy / WorkBuddy 上游。
//!
//! 后端 `https://copilot.tencent.com/v2/chat/completions` 本身就是标准 OpenAI
//! chat/completions 协议（含原生 tools / tool_calls / SSE 流式），因此：
//! - `prepare_chat` 直通 body，只需注入鉴权头；
//! - `translate_stream` 是恒等转发。

use serde_json::Value;

use crate::auth::CredentialManager;
use crate::models::get_available_models;

use super::{passthrough_stream, ByteStream, Upstream, UpstreamRequest};

pub struct CodeBuddyUpstream {
    backend: String,
    cred: CredentialManager,
}

impl CodeBuddyUpstream {
    pub fn new(cred: CredentialManager, backend: Option<String>) -> Self {
        Self {
            backend: backend.unwrap_or_else(|| crate::auth::BACKEND.to_string()),
            cred,
        }
    }
}

#[async_trait::async_trait]
impl Upstream for CodeBuddyUpstream {
    fn name(&self) -> &str {
        "codebuddy"
    }

    async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String> {
        let headers = self.cred.get_headers().await?;
        Ok(UpstreamRequest {
            url: format!("{}/v2/chat/completions", self.backend),
            headers,
            body: chat.clone(),
        })
    }

    fn translate_stream(&self, raw: ByteStream) -> ByteStream {
        passthrough_stream(raw)
    }

    fn aggregate(&self, raw: &[u8]) -> Value {
        super::aggregate_openai_sse(raw)
    }

    async fn models(&self) -> Vec<String> {
        get_available_models()
    }

    async fn health_info(&self) -> Value {
        match self.cred.summary().await {
            Ok(s) => serde_json::json!({"credential": s}),
            Err(e) => serde_json::json!({"credential_error": e}),
        }
    }
}
